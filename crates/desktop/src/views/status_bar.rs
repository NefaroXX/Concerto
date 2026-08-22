use iced::widget::{button, container, row, text};
use iced::{Background, Element, Length};

use concerto_config::CredentialStore;
use concerto_core::intent::RunStage;
use concerto_providers::provider_defs::{provider_definition, provider_readiness};

use crate::app::{App, Message, Page, RunStatus};
use crate::views::spend::{spend_chip_state, SpendChipTone};

fn page_name(page: Page) -> &'static str {
    match page {
        Page::Chat => "Chat",
        Page::ToolLog => "Tool Log",
        Page::DiffViewer => "Diff Viewer",
        Page::Settings => "Settings",
        Page::OrchestrationStudio => "Orchestration Studio",
        Page::Editor => "Editor",
    }
}

fn shortcut_hints(page: Page) -> &'static str {
    match page {
        Page::Chat => "Ctrl+T New Task  |  Ctrl+Enter Send  |  Ctrl+S Screenshot",
        Page::ToolLog => "Ctrl+L Tool Log",
        Page::DiffViewer => "Ctrl+D Diff  |  Ctrl+` Terminal",
        Page::Settings => "",
        Page::OrchestrationStudio => "",
        Page::Editor => "Ctrl+S Save  |  Ctrl+F Find  |  Ctrl+G Go to Line  |  Ctrl+Z Undo",
    }
}

/// Chip label for an intent-router [`RunStage`] (ADR-55 Phase 2a).
///
/// The labels describe what the agent is doing in user terms rather than the
/// router's internal stage names.
fn run_stage_label(stage: RunStage) -> String {
    match stage {
        RunStage::Understand => "Responding".to_string(),
        RunStage::Inspect => "Inspecting".to_string(),
        RunStage::Plan => "Planning".to_string(),
        RunStage::Execute => "Editing".to_string(),
        RunStage::Verify => "Testing".to_string(),
        RunStage::Complete => "Complete".to_string(),
        // `RunStage` is non-exhaustive (concerto-core); render unknown future
        // stages by their enum name rather than panicking or going blank.
        other => other.to_string(),
    }
}

/// True while any capability/intent/ack/plan dialog is waiting on the user.
///
/// A pending dialog means the run is blocked mid-stage, so the chip reports
/// "Waiting" instead of the current stage name. Mirrors the dialog-queue
/// checks the App performs when rendering the overlays.
fn dialog_waiting(app: &App) -> bool {
    !app.cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        || app.pending_ack.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        || !app.pending_intent.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        || !app.pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

/// Chip label to render in the status bar, or `None` to hide the chip.
///
/// Visible only while a run is actually in progress (`Running` with a known
/// stage) — the chip is absent at idle, while cancelling, and before the first
/// stage event of a run arrives.
pub(crate) fn run_stage_chip_label(app: &App) -> Option<String> {
    if app.run_status != RunStatus::Running {
        return None;
    }
    let stage = app.run_stage?;
    if dialog_waiting(app) {
        Some("Waiting".to_string())
    } else {
        Some(run_stage_label(stage))
    }
}

pub fn status_bar_view(app: &App) -> Element<'_, Message> {
    let ts = &app.current_theme.type_scale;
    let sp = &app.current_theme.spacing;
    let palette = &app.current_theme.palette;

    let name = text(page_name(app.page)).size(ts.caption);
    let hints = text(shortcut_hints(app.page)).size(ts.caption).color(palette.text_muted);

    // Screenshot button — monochrome Unicode
    let screenshot_btn = button(text("◻").size(ts.caption))
        .padding([4, 10])
        .on_press(Message::TakeScreenshot)
        .style(crate::ui::button::secondary);

    // Screenshot status (if any)
    let status_section = if let Some(ref status) = app.screenshot_status {
        row![screenshot_btn, text(status).size(ts.caption).color(palette.text_muted),]
            .spacing(sp.sm)
            .align_y(iced::Alignment::Center)
    } else {
        row![screenshot_btn].align_y(iced::Alignment::Center)
    };

    // Active project folder — click to change it.
    let folder_label = {
        let s = app.project_dir.to_string_lossy().to_string();
        let char_count = s.chars().count();
        if char_count > 42 {
            let tail: String = s.chars().skip(char_count - 41).collect();
            format!("…{tail}")
        } else {
            s
        }
    };
    let folder_btn = button(text(format!("▸ {}", folder_label)).size(ts.caption))
        .padding([4, 10])
        .style(crate::ui::button::secondary)
        .on_press(Message::OpenProjectDirPicker);

    let run_section: Element<'_, Message> = match app.run_status {
        RunStatus::Idle => text("Idle").size(ts.caption).into(),
        RunStatus::Running => row![
            text("Running").size(ts.caption),
            button(text("Stop").size(ts.caption))
                .padding([4, 10])
                .style(crate::ui::button::danger)
                .on_press(Message::CancelAgentRun),
        ]
        .spacing(sp.sm)
        .align_y(iced::Alignment::Center)
        .into(),
        RunStatus::Cancelling => text("Cancelling…").size(ts.caption).into(),
    };

    // Active provider / model + readiness summary.
    let provider_summary: Element<'_, Message> =
        match app.settings.providers.iter().find(|p| p.id == app.active_provider_id) {
            Some(provider) => {
                let def = provider_definition(&provider.provider);
                let creds = CredentialStore::new();
                let has_key = creds.exists(&provider.keyring_key);
                let ready = provider_readiness(provider, &def, has_key).is_ready();
                let model = if app.active_model.is_empty() { "—" } else { &app.active_model };
                let marker = if ready { "✓" } else { "⚠ setup" };
                let color = if ready { palette.success } else { palette.warning };
                row![
                    text(format!("{} · {}", provider.name, model)).size(ts.caption),
                    text(marker).size(ts.caption).color(color),
                ]
                .spacing(sp.sm)
                .align_y(iced::Alignment::Center)
                .into()
            }
            None => text("No provider selected").size(ts.caption).color(palette.warning).into(),
        };

    // Intent-router run-stage chip (ADR-55 Phase 2a), between the run
    // indicator and the transient feedback. Shows the current stage in
    // user-facing terms, or "Waiting" while a dialog blocks the run. Hidden
    // when idle / no stage yet. Rendered as a non-actionable button so it
    // mirrors the spend chip's subtle disabled-chip look.
    let run_stage_chip: Element<'_, Message> = match run_stage_chip_label(app) {
        Some(label) => {
            let color = if label == "Waiting" { palette.warning } else { palette.text };
            button(text(label).size(ts.caption).color(color))
                .padding([4, 10])
                .style(crate::ui::button::secondary)
                .into()
        }
        None => container(text("")).height(0).into(),
    };

    // Transient save feedback (auto-clears via Message::ClearSaveFeedback).
    let feedback: Element<'_, Message> = match &app.save_feedback {
        Some(msg) => text(msg).size(ts.caption).color(palette.success).into(),
        None => container(text("")).height(0).into(),
    };

    // ADR-59 D5: persistent config-load failure badge. Rendered (icon + text,
    // danger palette) whenever a config load fell back or a reload failed, so
    // load failures are visible at every load point — never color alone.
    let config_broken_badge: Element<'_, Message> = if app.config_broken {
        button(text("⚠ config").size(ts.caption).color(palette.danger))
            .padding([4, 10])
            .style(crate::ui::button::secondary)
            .into()
    } else {
        container(text("")).height(0).into()
    };

    // Live session spend chip — opens the Spend Log modal. Color follows the
    // cap thresholds: normal → text, >=80% of cap → warning, >=100% /
    // cap-exceeded event → danger.
    let spend_style = spend_chip_state(app.live_session_cost, app.session_cap, &app.cap_state);
    let spend_color = match spend_style.tone {
        SpendChipTone::Normal => palette.text,
        SpendChipTone::Warning => palette.warning,
        SpendChipTone::Danger => palette.danger,
    };
    let spend_btn = button(text(spend_style.label).size(ts.caption).color(spend_color))
        .padding([4, 10])
        .on_press(Message::OpenSpendLog)
        .style(crate::ui::button::secondary);

    let content = row![
        config_broken_badge,
        folder_btn,
        name,
        provider_summary,
        hints,
        run_section,
        run_stage_chip,
        feedback,
        status_section,
        spend_btn
    ]
    .spacing(16)
    .padding([sp.sm, sp.md])
    .align_y(iced::Alignment::Center);

    let bg = Background::Color(app.current_theme.palette.surface_variant);

    container(content)
        .width(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(bg),
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::widgets::capability_dialog::PendingIntent;
    use concerto_core::intent::RunStage;

    #[test]
    fn stage_labels_map_to_chip_text() {
        assert_eq!(run_stage_label(RunStage::Understand), "Responding");
        assert_eq!(run_stage_label(RunStage::Inspect), "Inspecting");
        assert_eq!(run_stage_label(RunStage::Plan), "Planning");
        assert_eq!(run_stage_label(RunStage::Execute), "Editing");
        assert_eq!(run_stage_label(RunStage::Verify), "Testing");
        assert_eq!(run_stage_label(RunStage::Complete), "Complete");
    }

    #[test]
    fn run_stage_chip_hidden_when_idle_or_stage_unknown() {
        // Idle: no stage yet.
        let (app, _) = App::new();
        assert_eq!(app.run_status, RunStatus::Idle);
        assert_eq!(run_stage_chip_label(&app), None);

        // Running but no stage event has landed yet.
        let (mut app, _) = App::new();
        app.run_status = RunStatus::Running;
        assert_eq!(run_stage_chip_label(&app), None);

        // A stale stage with the run not running must stay hidden.
        let (mut app, _) = App::new();
        app.run_stage = Some(RunStage::Plan);
        assert_eq!(run_stage_chip_label(&app), None);
    }

    #[test]
    fn run_stage_chip_shows_stage_label_while_running() {
        let (mut app, _) = App::new();
        app.run_status = RunStatus::Running;
        app.run_stage = Some(RunStage::Execute);
        assert_eq!(run_stage_chip_label(&app), Some("Editing".to_string()));
    }

    #[test]
    fn run_stage_chip_reports_waiting_when_dialog_pending() {
        let (mut app, _) = App::new();
        app.run_status = RunStatus::Running;
        app.run_stage = Some(RunStage::Inspect);

        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.pending_intent.lock().unwrap_or_else(|e| e.into_inner()).push_back(PendingIntent {
            question: "apply the stored plan?".into(),
            options: Vec::new(),
            sender: tx,
        });

        assert_eq!(run_stage_chip_label(&app), Some("Waiting".to_string()));
    }

    /// `status_bar_view` renders without panicking both with the run-stage
    /// chip hidden (idle) and with it set (running + stage).
    #[test]
    fn status_bar_renders_with_and_without_run_stage() {
        let (app, _) = App::new();
        let _ = status_bar_view(&app);

        let (mut app, _) = App::new();
        app.run_status = RunStatus::Running;
        app.run_stage = Some(RunStage::Verify);
        let _ = status_bar_view(&app);
    }

    /// ADR-59 D5: the persistent config-broken badge renders without
    /// panicking when the flag is set (startup fallback / reload failure).
    #[test]
    fn status_bar_renders_config_broken_badge() {
        let (mut app, _) = App::new();
        app.config_broken = true;
        let _ = status_bar_view(&app);
    }
}
