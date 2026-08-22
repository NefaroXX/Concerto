use std::fmt;
use std::path::PathBuf;

use iced::widget::{button, checkbox, column, container, pick_list, row, text, text_input, Column};
use iced::{Element, Length};

use concerto_config::managed::ManagedRuntimeManager;
use concerto_config::shell::{ProfileAvailability, ShellBackendType, ShellProfileConfig};
use concerto_config::ShellSettings;

use crate::theme::AppTheme;
use crate::ui::{form_field, SPACING_MD, SPACING_SM, SPACING_XS};

use super::helpers::{
    managed_export, managed_import, managed_install, managed_remove, managed_verify,
    test_shell_profile,
};
use super::{Message, State, WorkingDirBehaviorChoice, WORKING_DIR_CHOICES};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellProfileOption {
    id: String,
    name: String,
}

impl fmt::Display for ShellProfileOption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)
    }
}

impl State {
    pub(crate) fn handle_shell_message(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            // ── ADR-28 shell profiles ───────────────────────────────────────
            Message::ShellActiveProfileChanged(id) => {
                self.shell_active_profile = id;
            }
            Message::ShellProfileSelected(idx) => {
                self.selected_shell_profile =
                    if idx < self.shell_profiles.len() { Some(idx) } else { None };
            }
            Message::ShellProfileExecutableChanged(v) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.executable = v;
                }
            }
            Message::ShellProfileArgsChanged(v) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.args = v
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
            Message::ShellProfileEnvKeyChanged(i, v) => {
                if let Some(p) = self.selected_profile_mut() {
                    let keys: Vec<String> = p.env.keys().cloned().collect();
                    if let Some(old) = keys.get(i) {
                        if let Some(val) = p.env.remove(old) {
                            p.env.insert(v, val);
                        }
                    }
                }
            }
            Message::ShellProfileEnvValueChanged(i, v) => {
                if let Some(p) = self.selected_profile_mut() {
                    let keys: Vec<String> = p.env.keys().cloned().collect();
                    if let Some(k) = keys.get(i) {
                        p.env.insert(k.clone(), v);
                    }
                }
            }
            Message::ShellProfileAddEnv => {
                let key = self.shell_new_env_key.trim().to_string();
                let value = self.shell_new_env_value.trim().to_string();
                if let Some(p) = self.selected_profile_mut() {
                    if !key.is_empty() {
                        p.env.insert(key, value);
                        self.shell_new_env_key.clear();
                        self.shell_new_env_value.clear();
                    }
                }
            }
            Message::ShellProfileRemoveEnv(i) => {
                if let Some(p) = self.selected_profile_mut() {
                    let keys: Vec<String> = p.env.keys().cloned().collect();
                    if let Some(k) = keys.get(i) {
                        p.env.remove(k);
                    }
                }
            }
            Message::ShellNewEnvKeyChanged(v) => self.shell_new_env_key = v,
            Message::ShellNewEnvValueChanged(v) => self.shell_new_env_value = v,
            Message::ShellProfilePathAddChanged(v) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.path_additions = v
                        .split(',')
                        .map(|s| PathBuf::from(s.trim()))
                        .filter(|p| !p.as_os_str().is_empty())
                        .collect();
                }
            }
            Message::ShellProfileWorkingDirChanged(choice) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.default_working_dir = choice.to_behavior();
                }
            }
            Message::ShellProfileLoginToggled(v) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.login = v;
                }
            }
            Message::ShellProfileInteractiveToggled(v) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.interactive = v;
                }
            }
            Message::ShellProfileStartupChanged(v) => {
                if let Some(p) = self.selected_profile_mut() {
                    p.startup_script =
                        if v.trim().is_empty() { None } else { Some(PathBuf::from(v.trim())) };
                }
            }
            Message::ShellProfileAdd => {
                let mut suffix = 1;
                while self
                    .shell_profiles
                    .iter()
                    .any(|profile| profile.id == format!("custom-{suffix}"))
                {
                    suffix += 1;
                }
                let profile = ShellProfileConfig {
                    id: format!("custom-{suffix}"),
                    name: "New profile".into(),
                    backend: ShellBackendType::System,
                    executable: "bash".into(),
                    status: ProfileAvailability::Unknown,
                    ..Default::default()
                };
                self.shell_profiles.push(profile);
                self.selected_shell_profile = Some(self.shell_profiles.len() - 1);
            }
            Message::ShellProfileRemove(idx) => {
                if idx < self.shell_profiles.len() {
                    let removed_active = self.shell_profiles[idx].id == self.shell_active_profile;
                    self.shell_profiles.remove(idx);
                    if removed_active {
                        self.shell_active_profile = self
                            .shell_profiles
                            .first()
                            .map(|profile| profile.id.clone())
                            .unwrap_or_default();
                    }
                    match self.selected_shell_profile {
                        Some(sel) if sel == idx => self.selected_shell_profile = None,
                        Some(sel) if sel > idx => self.selected_shell_profile = Some(sel - 1),
                        _ => {}
                    }
                }
            }
            Message::ShellProfileTest(idx) => {
                let profile = self.shell_profiles.get(idx).cloned();
                return iced::Task::perform(
                    async move { test_shell_profile(profile) },
                    move |(available, detail)| Message::ShellProfileTestResult {
                        index: idx,
                        available,
                        detail,
                    },
                );
            }
            Message::ShellProfileTestResult { index, available, detail } => {
                if let Some(p) = self.shell_profiles.get_mut(index) {
                    p.status = if available {
                        ProfileAvailability::Available
                    } else {
                        ProfileAvailability::Unavailable(detail.clone())
                    };
                }
                self.shell_test_result = Some((index, detail));
            }

            // ── ADR-28 Slice 2: Managed Bash runtime management ───────────────
            Message::ShellManagedSourceChanged(v) => self.shell_managed_source = v,
            Message::ShellManagedExportPathChanged(v) => self.shell_managed_export_path = v,
            Message::ShellManagedImportPathChanged(v) => self.shell_managed_import_path = v,
            Message::ShellManagedInstall => {
                let src = self.shell_managed_source.clone();
                return iced::Task::perform(
                    async move { managed_install(src) },
                    Message::ShellManagedResult,
                );
            }
            Message::ShellManagedRemove => {
                return iced::Task::perform(
                    async { managed_remove() },
                    Message::ShellManagedResult,
                );
            }
            Message::ShellManagedVerify => {
                return iced::Task::perform(
                    async { managed_verify() },
                    Message::ShellManagedResult,
                );
            }
            Message::ShellManagedExport => {
                let p = self.shell_managed_export_path.clone();
                return iced::Task::perform(
                    async move { managed_export(p) },
                    Message::ShellManagedResult,
                );
            }
            Message::ShellManagedImport => {
                let p = self.shell_managed_import_path.clone();
                return iced::Task::perform(
                    async move { managed_import(p) },
                    Message::ShellManagedResult,
                );
            }
            Message::ShellManagedResult(msg) => {
                self.shell_managed_result = Some(msg);
                // Refresh the detected list so an installed managed shell
                // appears and a removed one disappears immediately.
                let settings = ShellSettings::new(
                    std::mem::take(&mut self.shell_profiles),
                    self.shell_active_profile.clone(),
                    None,
                )
                .normalized_for_host();
                self.shell_active_profile = settings.selected_profile_id().to_owned();
                self.shell_profiles = settings.profiles;
                self.selected_shell_profile = None;
            }

            _ => {}
        }
        iced::Task::none()
    }

    fn selected_profile_mut(&mut self) -> Option<&mut ShellProfileConfig> {
        match self.selected_shell_profile {
            Some(idx) if idx < self.shell_profiles.len() => self.shell_profiles.get_mut(idx),
            _ => None,
        }
    }

    /// Shell settings surface: choose the agent execution shell, then manage
    /// and edit executable/env/PATH/working-dir per profile.
    pub(super) fn shell_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let profile_options = self
            .shell_profiles
            .iter()
            .map(|profile| ShellProfileOption {
                id: profile.id.clone(),
                name: profile.name.clone(),
            })
            .collect::<Vec<_>>();
        let selected_option =
            profile_options.iter().find(|profile| profile.id == self.shell_active_profile).cloned();

        let bindings = column![form_field(
            theme,
            "Agent execution shell",
            false,
            Some("The shell agents use for commands. Validation and the integrated terminal use the same profile."),
            None::<&str>,
            pick_list(
                profile_options,
                selected_option,
                |profile| Message::ShellActiveProfileChanged(profile.id),
            ),
        )]
        .spacing(SPACING_SM);

        let mut profile_rows: Vec<Element<'_, Message>> = Vec::new();
        for (i, p) in self.shell_profiles.iter().enumerate() {
            let status: Element<'_, Message> = match &p.status {
                ProfileAvailability::Available => {
                    text("ready").size(11).color(palette.success).into()
                }
                ProfileAvailability::Unavailable(r) => {
                    text(format!("unavailable: {r}")).size(11).color(palette.danger).into()
                }
                ProfileAvailability::Unknown => {
                    text("not checked").size(11).color(palette.text_muted).into()
                }
                _ => text("unknown").size(11).color(palette.text_muted).into(),
            };
            let row_content = row![
                text(&p.name).size(13),
                text(format!("({})", p.executable)).size(11).color(palette.text_muted),
                status,
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            let btn = button(row_content)
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::ShellProfileSelected(i));
            profile_rows.push(btn.into());
        }
        profile_rows.push(
            button(text("Add profile").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::ShellProfileAdd)
                .into(),
        );

        let editor: Element<'_, Message> = match self.selected_shell_profile {
            Some(idx) => match self.shell_profiles.get(idx) {
                Some(p) => {
                    let p = p.clone();
                    let new_env_key = self.shell_new_env_key.clone();
                    let new_env_val = self.shell_new_env_value.clone();
                    let args_text = p.args.join(", ");
                    let path_text = p
                        .path_additions
                        .iter()
                        .map(|pb| pb.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let startup_text = p
                        .startup_script
                        .as_ref()
                        .map(|pb| pb.to_string_lossy().into_owned())
                        .unwrap_or_default();

                    let mut env_rows: Vec<Element<'_, Message>> = Vec::new();
                    let keys: Vec<String> = p.env.keys().cloned().collect();
                    for (i, k) in keys.iter().enumerate() {
                        let v = p.env.get(k).cloned().unwrap_or_default();
                        let ki = i;
                        let vi = i;
                        env_rows.push(
                            row![
                                text_input("key", k)
                                    .on_input(move |s| Message::ShellProfileEnvKeyChanged(ki, s))
                                    .width(160),
                                text_input("value", &v)
                                    .on_input(move |s| Message::ShellProfileEnvValueChanged(vi, s))
                                    .width(240),
                                button(text("Remove").size(13))
                                    .style(crate::ui::button::danger_outline)
                                    .padding([6, 14])
                                    .on_press(Message::ShellProfileRemoveEnv(i)),
                            ]
                            .spacing(8)
                            .align_y(iced::Alignment::Center)
                            .into(),
                        );
                    }
                    env_rows.push(
                        row![
                            text_input("key", &new_env_key)
                                .on_input(Message::ShellNewEnvKeyChanged)
                                .width(160),
                            text_input("value", &new_env_val)
                                .on_input(Message::ShellNewEnvValueChanged)
                                .width(240),
                            button(text("Add").size(13))
                                .style(crate::ui::button::secondary)
                                .padding([6, 14])
                                .on_press(Message::ShellProfileAddEnv),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center)
                        .into(),
                    );

                    let working_dir =
                        WorkingDirBehaviorChoice::from_behavior(&p.default_working_dir);

                    let availability = p.availability();
                    let status_color = match availability {
                        ProfileAvailability::Available => palette.success,
                        ProfileAvailability::Unavailable(_) => palette.danger,
                        ProfileAvailability::Unknown => palette.text_muted,
                        _ => palette.text_muted,
                    };
                    let status_label = match availability {
                        ProfileAvailability::Available => "Available".to_string(),
                        ProfileAvailability::Unavailable(ref r) => format!("Unavailable: {r}"),
                        ProfileAvailability::Unknown => "Availability unknown".to_string(),
                        _ => "Unknown".to_string(),
                    };
                    let test_detail: Option<String> = match &self.shell_test_result {
                        Some((test_idx, detail)) if *test_idx == idx => Some(detail.clone()),
                        _ => None,
                    };

                    column![
                        row![
                            text(p.name.clone()).size(14),
                            button(text("Test").size(13))
                                .style(crate::ui::button::secondary)
                                .padding([6, 14])
                                .on_press(Message::ShellProfileTest(idx)),
                            button(text("Remove profile").size(13))
                                .style(crate::ui::button::danger_outline)
                                .padding([6, 14])
                                .on_press(Message::ShellProfileRemove(idx)),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                        text(format!("● {status_label}")).size(12).color(status_color),
                        test_detail
                            .map(|d| -> Element<'a, Message> {
                                Element::from(text(d).size(11).color(palette.text_muted))
                            })
                            .unwrap_or_else(|| Element::from(text("").size(1))),
                        form_field(
                            theme,
                            "Executable",
                            false,
                            None::<&str>,
                            None::<&str>,
                            text_input("e.g. bash, /usr/bin/zsh", &p.executable)
                                .on_input(Message::ShellProfileExecutableChanged),
                        ),
                        form_field(
                            theme,
                            "Launch args (comma-separated)",
                            false,
                            None::<&str>,
                            None::<&str>,
                            text_input("args", &args_text)
                                .on_input(Message::ShellProfileArgsChanged),
                        ),
                        form_field(
                            theme,
                            "PATH additions (comma-separated)",
                            false,
                            None::<&str>,
                            None::<&str>,
                            text_input("dirs", &path_text)
                                .on_input(Message::ShellProfilePathAddChanged),
                        ),
                        form_field(
                            theme,
                            "Working directory",
                            false,
                            None::<&str>,
                            None::<&str>,
                            pick_list(
                                WORKING_DIR_CHOICES,
                                Some(working_dir),
                                Message::ShellProfileWorkingDirChanged,
                            ),
                        ),
                        row![
                            checkbox(p.login)
                                .label("Login shell")
                                .on_toggle(Message::ShellProfileLoginToggled),
                            checkbox(p.interactive)
                                .label("Interactive")
                                .on_toggle(Message::ShellProfileInteractiveToggled),
                        ]
                        .spacing(SPACING_MD),
                        form_field(
                            theme,
                            "Startup script",
                            false,
                            None::<&str>,
                            None::<&str>,
                            text_input("path", &startup_text)
                                .on_input(Message::ShellProfileStartupChanged),
                        ),
                        text("Environment").size(12).color(palette.text_muted),
                        Column::with_children(env_rows).spacing(SPACING_XS),
                    ]
                    .spacing(SPACING_SM)
                    .into()
                }
                None => {
                    container(text("Select a profile to edit.").size(12).color(palette.text_muted))
                        .into()
                }
            },
            None => container(text("Select a profile to edit.").size(12).color(palette.text_muted))
                .into(),
        };

        // ADR-28 Slice 2 — Managed Bash runtime management block. The manager is
        // the source of truth; this surfaces install/remove/verify/export/import
        // and a live install status.
        let managed_status = match ManagedRuntimeManager::auto_detect() {
            Some(m) => format!(
                "Installed: {}  (integrity hash {}…)",
                m.version,
                &m.runtime_integrity.hash.chars().take(12).collect::<String>()
            ),
            None => "Not installed".to_string(),
        };
        let managed_result: Element<'_, Message> = match &self.shell_managed_result {
            Some(s) => text(s).size(11).color(palette.text_muted).into(),
            None => Element::from(text("").size(1)),
        };
        let managed_block = column![
            text("Concerto Managed Bash (ADR-28 Slice 2)").size(13).color(palette.text),
            text(managed_status).size(11).color(palette.text_muted),
            row![
                text_input("source bash path", &self.shell_managed_source)
                    .on_input(Message::ShellManagedSourceChanged)
                    .width(Length::Fill),
                button(text("Install").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::ShellManagedInstall),
                button(text("Remove").size(13))
                    .style(crate::ui::button::danger_outline)
                    .padding([6, 14])
                    .on_press(Message::ShellManagedRemove),
                button(text("Verify").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::ShellManagedVerify),
            ]
            .spacing(SPACING_XS)
            .align_y(iced::Alignment::Center),
            row![
                text_input("export manifest path", &self.shell_managed_export_path)
                    .on_input(Message::ShellManagedExportPathChanged)
                    .width(Length::Fill),
                button(text("Export").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::ShellManagedExport),
            ]
            .spacing(SPACING_XS)
            .align_y(iced::Alignment::Center),
            row![
                text_input("import manifest path", &self.shell_managed_import_path)
                    .on_input(Message::ShellManagedImportPathChanged)
                    .width(Length::Fill),
                button(text("Import").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::ShellManagedImport),
            ]
            .spacing(SPACING_XS)
            .align_y(iced::Alignment::Center),
            managed_result,
        ]
        .spacing(SPACING_SM);

        column![
            text("Choose the shell used by agents. Installed shells are detected automatically; add a profile only for a custom executable or environment.")
                .size(12)
                .color(palette.text_muted),
            managed_block,
            bindings,
            Column::with_children(profile_rows).spacing(SPACING_XS),
            editor,
        ]
        .spacing(SPACING_SM)
        .into()
    }
}
