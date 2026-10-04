//! Project-first desktop shell.

use std::path::{Path, PathBuf};

use camino::Utf8PathBuf;
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
    /// Window-close guard: write every dirty editor buffer, then continue
    /// the close only when every write succeeded.
    ExitGuardSaveAll,
    /// Window-close guard: drop all unsaved edits and continue the close.
    ExitGuardDiscardAll,
    /// Window-close guard: abort the close and keep editing.
    ExitGuardCancel,
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

/// Unsaved editor documents that block a window close until the user chooses
/// Save All / Discard All / Cancel. Dirty buffers can live in background tabs,
/// so they are invisible from the active document alone.
struct ExitGuard {
    /// Dirty documents, in tab order, shown in the modal.
    dirty: Vec<Utf8PathBuf>,
    /// Last Save All failure summary, if any.
    error: Option<String>,
}

/// What a window-close request should do next. Dirty editor buffers take
/// precedence over the ADR-60 D7 run-cancel wait: unsaved work must be
/// resolved before any close path runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseDecision {
    /// Exit immediately (no project open, or no run in flight).
    Exit,
    /// Cancel the run and wait, bounded, before exiting.
    WaitForRun,
    /// Block the close behind the dirty-buffer guard.
    Guard,
}

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
    /// Active window-close guard, if the user tried to close with unsaved
    /// editor buffers. `None` means no close is pending.
    exit_guard: Option<ExitGuard>,
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
                    exit_guard: None,
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
                exit_guard: None,
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
            Message::CloseRequested => match self.close_decision() {
                CloseDecision::Guard => {
                    // Unsaved editor buffers survive in background tabs, so a
                    // window close must not silently drop them. Block the
                    // exit behind a Save All / Discard All / Cancel decision.
                    if let Some(app) = self.app.as_ref() {
                        let dirty = app.editor.dirty_files();
                        self.exit_guard = Some(ExitGuard { dirty, error: None });
                    }
                    Task::none()
                }
                CloseDecision::Exit | CloseDecision::WaitForRun => self.begin_close(),
            },
            Message::ExitAfterGracefulStop => iced::exit(),
            Message::ExitGuardSaveAll => {
                let Some(current) = self.app.as_mut() else {
                    self.exit_guard = None;
                    return Task::none();
                };
                let project_dir = Utf8PathBuf::from_path_buf(current.project_dir.clone())
                    .unwrap_or_else(|path| Utf8PathBuf::from(path.to_string_lossy().as_ref()));
                let app::App { editor, vfs, cancel_token, .. } = current;
                let failures = editor.save_all(vfs, &project_dir, cancel_token);
                if failures.is_empty() {
                    self.exit_guard = None;
                    self.begin_close()
                } else {
                    let detail = failures
                        .iter()
                        .map(|failure| format!("{}: {}", failure.path, failure.message))
                        .collect::<Vec<_>>()
                        .join("; ");
                    if let Some(guard) = self.exit_guard.as_mut() {
                        guard.error =
                            Some(format!("Could not save {} file(s): {detail}", failures.len()));
                    }
                    Task::none()
                }
            }
            Message::ExitGuardDiscardAll => {
                self.exit_guard = None;
                self.begin_close()
            }
            Message::ExitGuardCancel => {
                self.exit_guard = None;
                Task::none()
            }
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

    /// Decide how a window-close request is handled. Pure so the exit-guard
    /// precedence is unit-testable without an iced runtime.
    fn close_decision(&self) -> CloseDecision {
        let Some(app) = self.app.as_ref() else {
            return CloseDecision::Exit;
        };
        if !app.editor.dirty_files().is_empty() {
            return CloseDecision::Guard;
        }
        if app.is_run_active() {
            CloseDecision::WaitForRun
        } else {
            CloseDecision::Exit
        }
    }

    /// ADR-60 D7 (interrupt-safe resume): a close with a run in flight cancels
    /// it and waits, bounded, for the run to settle — the coordinator's cancel
    /// path persists the interrupted (completed=0) checkpoint BEFORE
    /// `run_shared_agent` returns, so a settled epoch bump implies durable
    /// resumable state. Exiting over the run (the previous behavior) was the
    /// desktop's hard-kill: the executor drops its runtime 750 ms after exit
    /// and nothing lands.
    fn begin_close(&mut self) -> Task<Message> {
        let Some(app) = self.app.as_ref() else {
            // No project open: nothing can be running — exit as before.
            return iced::exit();
        };
        let running = app.is_run_active();
        let settle = app.run_settle_epoch();
        app.cancel_token.cancel();
        if !running {
            iced::exit()
        } else {
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

    pub fn view(&self) -> Element<'_, Message> {
        let base = if self.chooser_open {
            self.project_chooser()
        } else {
            self.app
                .as_ref()
                .map(|app| app.view().map(Message::App))
                .unwrap_or_else(|| self.project_chooser())
        };

        let mut layered = base;

        // ADR-44 §4: overlay the consent gate over the chooser for the first
        // out-of-root open. Composed like the app-side system dialogs: a
        // centered modal card over a semi-transparent palette backdrop.
        if let Some(pending) = &self.pending_root_consent {
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
            layered = stack![layered, backdrop].into();
        }

        // Window-close guard sits above every other surface: an unresolved
        // close is a modal decision.
        if let Some(guard) = &self.exit_guard {
            layered = stack![layered, self.exit_guard_backdrop(guard)].into();
        }

        layered
    }

    /// The window-close guard modal: lists every dirty editor buffer and
    /// offers Save All / Discard All / Cancel. Palette-only colors.
    fn exit_guard_backdrop<'a>(&'a self, guard: &'a ExitGuard) -> Element<'a, Message> {
        let palette = &self.current_theme.palette;

        let mut files = Column::new().spacing(4);
        for path in &guard.dirty {
            files = files.push(text(path.as_str()).size(12).color(palette.text_muted));
        }

        let mut content = column![
            text("Unsaved changes").size(20).color(palette.text),
            text("These files have unsaved edits. Save them before closing?")
                .size(13)
                .color(palette.text_muted),
            container(scrollable(files)).max_height(220.0),
        ]
        .spacing(12);

        if let Some(error) = &guard.error {
            content = content.push(text(error).size(12).color(palette.danger));
        }

        content = content.push(
            row![
                button(text("Save All").size(13))
                    .style(crate::ui::button::primary)
                    .padding([8, 16])
                    .on_press(Message::ExitGuardSaveAll),
                button(text("Discard All").size(13))
                    .style(crate::ui::button::danger)
                    .padding([8, 16])
                    .on_press(Message::ExitGuardDiscardAll),
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([8, 16])
                    .on_press(Message::ExitGuardCancel),
            ]
            .spacing(10),
        );

        let card = container(content).width(520).padding(24).style(crate::ui::container::modal);

        container(card)
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
            })
            .into()
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
            exit_guard: None,
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
            exit_guard: None,
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
            exit_guard: None,
        };

        let _ = launcher.update(Message::OpenProject(target.path().to_path_buf()));
        let gate_app = launcher.app.as_ref().expect("app exists");
        assert!(gate_app.pending_root_consent.is_some(), "switch must be gated");
        assert!(!launcher.chooser_open, "chooser closes so the app gate modal shows");
        assert_eq!(launcher.error, None, "no false 'finish or cancel' error");
    }

    // -----------------------------------------------------------------------
    // Window-close guard — dirty editor buffers (active and background)
    // -----------------------------------------------------------------------

    /// A launcher whose app has two dirty editor tabs: `first` was edited and
    /// then parked in a background buffer, `second` is active and edited.
    fn launcher_with_two_dirty_tabs(
    ) -> Result<(DesktopApp, tempfile::TempDir, Utf8PathBuf, Utf8PathBuf), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| std::io::Error::other(format!("Non-UTF8 test path: {path:?}")))?;
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::write(&first, "fn first() {}\n")?;
        std::fs::write(&second, "fn second() {}\n")?;

        let (mut app, _) = app::App::new();
        let cancel = app.cancel_token.clone();
        app.editor.open_file(&first, &app.vfs);
        let _ = app.editor.update(editor_edit('x'), &app.vfs, &root, &cancel);
        app.editor.open_file(&second, &app.vfs);
        let _ = app.editor.update(editor_edit('y'), &app.vfs, &root, &cancel);
        assert_eq!(app.editor.dirty_files().len(), 2, "fixture must have two dirty tabs");

        let launcher = DesktopApp {
            app: Some(app),
            current_theme: AppTheme::by_name("Midnight"),
            registry: ProjectRegistry::default(),
            chooser_open: false,
            error: None,
            pending_root_consent: None,
            effective_roots: Vec::new(),
            exit_guard: None,
        };
        Ok((launcher, dir, first, second))
    }

    fn editor_edit(ch: char) -> crate::views::code_editor::Message {
        crate::views::code_editor::Message::Edit(iced::widget::text_editor::Action::Edit(
            iced::widget::text_editor::Edit::Insert(ch),
        ))
    }

    /// Closing with a dirty background tab must not exit; the guard lists
    /// every dirty file.
    #[test]
    fn close_requested_with_dirty_background_tab_shows_guard_without_exiting(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut launcher, _dir, first, second) = launcher_with_two_dirty_tabs()?;

        let _ = launcher.update(Message::CloseRequested);

        let guard = launcher.exit_guard.as_ref().expect("dirty tabs must arm the exit guard");
        assert_eq!(guard.dirty.len(), 2, "both dirty tabs must be listed");
        assert!(guard.dirty.contains(&first));
        assert!(guard.dirty.contains(&second));
        assert_eq!(launcher.close_decision(), CloseDecision::Guard, "close must be blocked");
        Ok(())
    }

    /// Cancel dismisses the guard and leaves both dirty buffers untouched.
    #[test]
    fn exit_guard_cancel_keeps_both_dirty_tabs() -> Result<(), Box<dyn std::error::Error>> {
        let (mut launcher, _dir, first, second) = launcher_with_two_dirty_tabs()?;
        let _ = launcher.update(Message::CloseRequested);

        let _ = launcher.update(Message::ExitGuardCancel);

        assert!(launcher.exit_guard.is_none(), "cancel must dismiss the guard");
        let app = launcher.app.as_ref().expect("app exists");
        assert_eq!(app.editor.dirty_files().len(), 2, "cancel must keep both dirty buffers");
        assert!(app.editor.dirty_files().contains(&first));
        assert!(app.editor.dirty_files().contains(&second));
        assert_eq!(std::fs::read_to_string(&first)?, "fn first() {}\n", "cancel must not write");
        assert_eq!(std::fs::read_to_string(&second)?, "fn second() {}\n");
        Ok(())
    }

    /// Save All writes the active and background tabs, then the close proceeds.
    #[test]
    fn exit_guard_save_all_writes_every_dirty_tab_then_allows_exit(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut launcher, _dir, first, second) = launcher_with_two_dirty_tabs()?;
        let _ = launcher.update(Message::CloseRequested);

        let _ = launcher.update(Message::ExitGuardSaveAll);

        assert!(launcher.exit_guard.is_none(), "successful Save All clears the guard");
        let app = launcher.app.as_ref().expect("app exists");
        assert!(app.editor.dirty_files().is_empty(), "no dirty buffers after Save All");
        assert_eq!(std::fs::read_to_string(&first)?, "xfn first() {}\n", "background tab saved");
        assert_eq!(std::fs::read_to_string(&second)?, "yfn second() {}\n", "active tab saved");
        assert_eq!(launcher.close_decision(), CloseDecision::Exit, "close proceeds after save");
        Ok(())
    }

    /// A failed write keeps the guard up and blocks the exit; other dirty
    /// tabs still save.
    #[test]
    fn exit_guard_save_all_failure_keeps_guard_and_blocks_exit(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut launcher, _dir, first, second) = launcher_with_two_dirty_tabs()?;
        // Sabotage the background tab's path so its write fails
        // deterministically (writing to a directory always fails).
        std::fs::remove_file(&first)?;
        std::fs::create_dir(&first)?;

        let _ = launcher.update(Message::CloseRequested);
        let _ = launcher.update(Message::ExitGuardSaveAll);

        let guard = launcher.exit_guard.as_ref().expect("failed save must keep the guard");
        assert!(guard.error.is_some(), "the failure must be surfaced");
        let app = launcher.app.as_ref().expect("app exists");
        assert!(app.editor.dirty_files().contains(&first), "failed tab stays dirty");
        assert_eq!(launcher.close_decision(), CloseDecision::Guard, "close stays blocked");
        assert_eq!(std::fs::read_to_string(&second)?, "yfn second() {}\n", "other tab saved");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ADR numbering integrity
    // -----------------------------------------------------------------------

    fn adr_number(name: &str) -> Option<&str> {
        let rest = name.strip_prefix("ADR-")?;
        let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
        (!digits.is_empty()).then_some(digits)
    }

    fn readme_adr_numbers(readme: &str) -> Vec<String> {
        let mut numbers = Vec::new();
        for line in readme.lines() {
            // Only the canonical registry rows: `| [<n>](./ADR-<n>-… )`.
            // Superseded/archive rows use `[ADR-<n>]` link text and are
            // cross-references, not registry entries.
            let Some(rest) = line.trim_start().strip_prefix("| [") else { continue };
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                continue;
            }
            if rest.starts_with(&format!("{digits}](./ADR-{digits}")) {
                numbers.push(digits);
            }
        }
        numbers
    }

    /// ADR numbers must be unique across filenames and README rows. Catches a
    /// second `ADR-78-*.md` merging alongside the first.
    #[test]
    fn adr_numbers_are_unique_in_filenames_and_readme() -> Result<(), Box<dyn std::error::Error>> {
        let adr_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/adrs");
        let mut filenames: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&adr_dir)? {
            let name = entry?.file_name();
            if let Some(number) = adr_number(&name.to_string_lossy()) {
                filenames.push(number.to_string());
            }
        }
        filenames.sort();
        for pair in filenames.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate ADR filename number {}", pair[0]);
        }

        let readme = std::fs::read_to_string(adr_dir.join("README.md"))?;
        let mut rows = readme_adr_numbers(&readme);
        rows.sort();
        for pair in rows.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate ADR README row number {}", pair[0]);
        }
        Ok(())
    }
}
