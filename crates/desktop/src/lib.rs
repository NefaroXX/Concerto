#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

pub mod app;
pub mod config_watch;
pub mod executor;
pub mod project_launcher;
pub mod root_consent;
pub mod runtime;
pub mod services;
pub mod shortcuts;
pub mod theme;
pub mod ui;
pub mod views;
pub mod widgets;

fn application_log_writer() -> tracing_subscriber::fmt::writer::BoxMakeWriter {
    let Some(path) = dirs::data_dir()
        .map(|directory| directory.join("concerto").join("logs").join("concerto.log"))
    else {
        return tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr);
    };
    if path.parent().is_some_and(|parent| std::fs::create_dir_all(parent).is_err()) {
        return tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr);
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => {
            tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::sync::Mutex::new(file))
        }
        Err(_) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr),
    }
}

/// Application entry point — called from `main.rs`.
pub fn run() -> iced::Result {
    // Default to WARN (not tracing's ERROR default) so the application log
    // captures warn-level diagnostics out of the box; RUST_LOG still overrides.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::Level::WARN.into())
                .from_env_lossy(),
        )
        .with_writer(application_log_writer())
        .init();

    iced::application(
        project_launcher::DesktopApp::new,
        project_launcher::DesktopApp::update,
        project_launcher::DesktopApp::view,
    )
    .title(project_launcher::DesktopApp::title)
    .subscription(project_launcher::DesktopApp::subscription)
    .theme(project_launcher::DesktopApp::theme)
    .exit_on_close_request(false)
    .executor::<executor::ShutdownExecutor>()
    .run()
}
