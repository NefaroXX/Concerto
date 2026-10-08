//! Skills tab view builders (Settings → Extensions → Skills).
//!
//! Pure relocation (NORM S27-A): the Skills cluster of the Extensions section
//! moves verbatim from `views/settings/mod.rs` into this file as an inherent
//! `impl State` block — the same pattern as [`super::ext_mcp_view`] and
//! [`super::shell`]. The cluster is the master–detail tab shell
//! (`ext_skills_tab`), the inert-while-busy Refresh / New-skill controls, the
//! per-skill row, the read-only detail pane with its CRUD controls, and the
//! create and edit wizard forms. No behavior, signature, call-site, or
//! [`super::Message`] shape change: the parent `view` dispatch
//! (`ExtensionTab::Skills => self.ext_skills_tab(theme)`) is untouched.
//!
//! The shared module-level helpers (`ext_meta_row`, `ext_master_detail`,
//! `path_summary`, `truncate`) stay in `mod.rs` and enter this file through
//! `use super::…`, so the sibling extension views keep rendering from a single
//! definition. Every member is `pub(super)`: from this child module `super` is
//! `views::settings`, so `pub(super)` resolves to `views::settings` plus its
//! descendants — exactly the effective scope a private item in `mod.rs` had.
//! The Skills view smoke test moves with the file (`ext_mcp_view` precedent);
//! the footer and full-page render tests stay in `mod.rs`.

use iced::widget::{button, checkbox, column, pick_list, row, text, text_editor, text_input};
use iced::{Alignment, Element, Length};

use concerto_api_types::extension::SkillDescriptor;

use crate::theme::AppTheme;
use crate::ui::{form_field, SPACING_SM, SPACING_XS};

use super::{
    ext_master_detail, ext_meta_row, path_summary, truncate, ExtensionTab, Message, SkillEditDraft,
    State, SKILL_DIAGNOSTIC_MAX_CHARS,
};

impl State {
    /// Shared refresh control for the Skills tab (inert while a discovery run
    /// is in flight; the label doubles as the spinner).
    pub(super) fn ext_refresh_button<'a>(&'a self) -> Element<'a, Message> {
        if self.skills_loading {
            button(text("Discovering…").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Refresh").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::SkillsDiscoveryRequested)
                .into()
        }
    }

    // ── Skills tab ───────────────────────────────────────────────────────
    pub(super) fn ext_skills_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let mut master: Vec<Element<'a, Message>> = vec![
            checkbox(self.skills_enabled)
                .label("Enable skills")
                .on_toggle(Message::SkillsEnabledToggled)
                .into(),
            ext_meta_row(theme, "Search paths", path_summary(&self.skills_search_paths, true)),
            ext_meta_row(theme, "Auto-load", if self.skills_auto_load { "on" } else { "off" }),
            ext_meta_row(
                theme,
                "Prompt budget",
                self.skills_max_chars
                    .map(|chars| chars.to_string())
                    .unwrap_or_else(|| "default (orchestrator budget)".to_string()),
            ),
            row![
                text("Discovered skills").size(13).color(palette.text),
                self.ext_new_skill_button(),
                self.ext_refresh_button(),
            ]
            .spacing(SPACING_SM)
            .align_y(Alignment::Center)
            .into(),
        ];

        if let Some(error) = &self.skills_error {
            master.push(text(error).size(12).color(palette.danger).into());
        } else if self.skills_discovered.is_empty() {
            master.push(
                text(if self.skills_loaded {
                    "No skills discovered under the configured search paths."
                } else if self.skills_loading {
                    "Discovering skills…"
                } else {
                    "Skills have not been discovered yet. Press Refresh."
                })
                .size(12)
                .color(palette.text_muted)
                .into(),
            );
        } else {
            for skill in &self.skills_discovered {
                master.push(self.ext_skill_row(theme, skill));
            }
        }

        // Per-path/per-pack diagnostics from the last discovery run (missing
        // search paths, packs skipped for malformed manifests) explain a
        // sparse result without hiding it behind a bare "no skills found" line.
        // Each line is truncated so one long absolute path cannot balloon the
        // pane.
        for warning in &self.skills_warnings {
            master.push(
                text(format!("• {}", truncate(warning, SKILL_DIAGNOSTIC_MAX_CHARS)))
                    .size(11)
                    .color(palette.warning)
                    .into(),
            );
        }

        // Informational notes from the same run — e.g. a pack with an empty
        // manifest id loaded under its directory name (its manifest id was
        // missing, which is common for local packs) — are not failures, so
        // they render as quiet, muted one-liners under the warnings.
        for note in &self.skills_notes {
            master.push(
                text(truncate(note, SKILL_DIAGNOSTIC_MAX_CHARS))
                    .size(11)
                    .color(palette.text_muted)
                    .into(),
            );
        }

        ext_master_detail(
            theme,
            column(master).spacing(SPACING_SM).into(),
            if self.skill_create_open {
                self.ext_skill_create_form(theme)
            } else {
                self.ext_skill_detail(theme)
            },
        )
    }

    /// Master "New skill" action (ADR-43): opens the create wizard in the
    /// detail pane. Inert while the wizard is already open or a CRUD operation
    /// is in flight so the user cannot queue competing writes.
    pub(super) fn ext_new_skill_button<'a>(&'a self) -> Element<'a, Message> {
        let enabled = !self.skill_create_open && !self.skill_crud_busy;
        let mut button =
            button(text("New skill").size(13)).style(crate::ui::button::secondary).padding([6, 14]);
        if enabled {
            button = button.on_press(Message::SkillCreatePressed);
        }
        button.into()
    }

    pub(super) fn ext_skill_row<'a>(
        &'a self,
        theme: &'a AppTheme,
        skill: &'a SkillDescriptor,
    ) -> Element<'a, Message> {
        let active = self.ext_selected_skill.as_deref() == Some(skill.id.as_str());
        let checked = self.skills_allow_all || self.skills_enabled_ids.contains(&skill.id);
        let name = if skill.manifest.name.is_empty() {
            skill.id.clone()
        } else {
            skill.manifest.name.clone()
        };
        let version =
            if skill.manifest.version.is_empty() { "0.0.0" } else { &skill.manifest.version };
        let tool_count = skill.manifest.tools.len();
        let select_id = skill.id.clone();
        let toggle_id = skill.id.clone();

        row![
            crate::ui::list_item(
                theme,
                active,
                Message::ExtensionItemSelected(ExtensionTab::Skills, select_id),
                column![
                    text(name).size(13).color(theme.palette.text),
                    text(format!(
                        "v{version} · {n} tool{s}",
                        version = version,
                        n = tool_count,
                        s = if tool_count == 1 { "" } else { "s" },
                    ))
                    .size(11)
                    .color(theme.palette.text_muted),
                ]
                .spacing(2),
            ),
            checkbox(checked)
                .label("")
                .on_toggle(move |on| Message::SkillTogglePressed(toggle_id.clone(), on)),
        ]
        .spacing(SPACING_XS)
        .align_y(Alignment::Center)
        .into()
    }

    pub(super) fn ext_skill_detail<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let Some(skill) = self
            .skills_discovered
            .iter()
            .find(|skill| self.ext_selected_skill.as_deref() == Some(skill.id.as_str()))
        else {
            return column![
                text("No skill selected").size(14).color(palette.text),
                text("Select a skill in the list to view its metadata and instructions.")
                    .size(12)
                    .color(palette.text_muted),
            ]
            .spacing(SPACING_SM)
            .into();
        };

        // Edit mode: replace the read-only metadata with the editable form.
        if self.skill_editing_id.as_deref() == Some(skill.id.as_str()) {
            if let Some(draft) = &self.skill_edit_draft {
                return self.ext_skill_edit_form(theme, skill, draft);
            }
        }

        let name = if skill.manifest.name.is_empty() {
            skill.id.clone()
        } else {
            skill.manifest.name.clone()
        };
        let version =
            if skill.manifest.version.is_empty() { "0.0.0" } else { &skill.manifest.version };
        let tools = if skill.manifest.tools.is_empty() {
            "none".to_string()
        } else {
            skill.manifest.tools.join(", ")
        };
        let description = if skill.manifest.description.is_empty() {
            "(no description)".to_string()
        } else {
            truncate(&skill.manifest.description, 160)
        };
        let expanded = self.skills_expanded.contains(&skill.id);

        let mut rows: Vec<Element<'a, Message>> = vec![
            text(name).size(16).color(palette.text).into(),
            ext_meta_row(theme, "ID", skill.id.as_str()),
            ext_meta_row(theme, "Version", version),
            ext_meta_row(theme, "Tools", tools),
            ext_meta_row(theme, "Description", description),
            ext_meta_row(theme, "Pack", skill.pack_dir.display().to_string()),
            row![button(
                text(if expanded { "Hide instructions" } else { "Show instructions" }).size(13)
            )
            .style(crate::ui::button::secondary)
            .padding([6, 14])
            .on_press(Message::SkillExpandToggled(skill.id.clone())),]
            .into(),
        ];
        if expanded {
            rows.push(crate::widgets::code_block::view(
                &skill.instructions,
                None,
                Message::SkillExpandToggled(skill.id.clone()),
                palette.surface_variant,
            ));
        }
        rows.push(self.ext_skill_crud_controls(theme, skill));
        column(rows).spacing(SPACING_SM).into()
    }

    /// Edit / Delete controls for a selected skill, plus the inline
    /// delete-confirm prompt and the last CRUD outcome line (ADR-43 edge).
    /// Deletion is destructive, so the first Delete press only arms a confirm
    /// prompt (same explicit-confirm rule as providers and MCP servers);
    /// confirming moves the manifest files to hidden `.deleted-*` backups in
    /// place, so the operation stays reversible. Packs without a `skill.toml`
    /// (SKILL.md-only) cannot be edited in the v1 editor and only offer Delete.
    pub(super) fn ext_skill_crud_controls<'a>(
        &'a self,
        theme: &'a AppTheme,
        skill: &'a SkillDescriptor,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;
        let mut rows: Vec<Element<'a, Message>> = Vec::new();

        if let Some(result) = &self.skill_crud_result {
            rows.push(
                text(result)
                    .size(11)
                    .color(if result.starts_with("Error") {
                        palette.danger
                    } else {
                        palette.success
                    })
                    .into(),
            );
        }

        if self.skill_delete_confirm.as_deref() == Some(skill.id.as_str()) {
            rows.push(
                row![
                    text("Delete this skill pack?").size(12).color(palette.warning),
                    button(text("Confirm").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 12])
                        .on_press(Message::SkillDeleteConfirmed(skill.id.clone())),
                    button(text("Cancel").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 12])
                        .on_press(Message::SkillDeleteCancelled(skill.id.clone())),
                ]
                .spacing(SPACING_XS)
                .align_y(Alignment::Center)
                .into(),
            );
            return column(rows).spacing(SPACING_XS).into();
        }

        let editable = skill.pack_dir.join("skill.toml").exists();
        let busy = self.skill_crud_busy;

        let mut edit_button = button(text("Edit skill").size(13))
            .style(crate::ui::button::secondary)
            .padding([6, 14]);
        if editable && !busy {
            edit_button = edit_button.on_press(Message::SkillEditPressed(skill.id.clone()));
        }

        let mut delete_button = button(text("Delete skill").size(13))
            .style(crate::ui::button::danger_outline)
            .padding([6, 14]);
        if !busy {
            delete_button = delete_button.on_press(Message::SkillDeletePressed(skill.id.clone()));
        }

        rows.push(row![edit_button, delete_button].spacing(SPACING_SM).into());
        if !editable {
            rows.push(
                text("This pack uses SKILL.md; edit it in the file directly.")
                    .size(11)
                    .color(palette.text_muted)
                    .into(),
            );
        }
        column(rows).spacing(SPACING_XS).into()
    }

    /// Full create wizard for a brand-new `skill.toml` pack (ADR-43), rendered
    /// in the detail pane in place of the read-only metadata. Id and parent are
    /// validated inline and again on Confirm; a success lands the pack on disk
    /// and triggers a discovery refresh so it appears immediately.
    pub(super) fn ext_skill_create_form<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let busy = self.skill_crud_busy;

        let create_button: Element<'a, Message> = if busy {
            button(text("Creating…").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Create skill").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .on_press(Message::SkillCreateConfirmed)
                .into()
        };

        let result_line: Element<'a, Message> = match &self.skill_crud_result {
            Some(result) if result.starts_with("Error") => {
                text(result).size(11).color(palette.danger).into()
            }
            Some(_) => iced::widget::Space::new().height(0).into(),
            None => iced::widget::Space::new().height(0).into(),
        };

        let instructions_label: Element<'a, Message> =
            text("Instructions").size(13).color(palette.text).into();
        let instructions_hint: Element<'a, Message> =
            text("The skill body, injected into prompts when the skill loads.")
                .size(11)
                .color(palette.text_muted)
                .into();

        column![
            text("New skill pack").size(16).color(palette.text),
            text(
                "Create a skill.toml in one of the configured search paths; the new pack \
                 appears in the discovered list after the refresh that follows a successful create."
            )
            .size(12)
            .color(palette.text_muted),
            form_field(
                theme,
                "Id",
                true,
                Some("Single path component, e.g. \"rust-testing\"; it becomes the pack directory name."),
                self.skill_create_id_error.as_deref(),
                text_input("skill id", &self.skill_create_id)
                    .on_input(Message::SkillCreateIdChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Name",
                false,
                Some("Blank uses the id."),
                None::<&str>,
                text_input("display name", &self.skill_create_name)
                    .on_input(Message::SkillCreateNameChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Version",
                false,
                Some("Blank uses 0.1.0."),
                None::<&str>,
                text_input("0.1.0", &self.skill_create_version)
                    .on_input(Message::SkillCreateVersionChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Description",
                false,
                None::<&str>,
                None::<&str>,
                text_input("short description", &self.skill_create_description)
                    .on_input(Message::SkillCreateDescriptionChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Parent directory",
                true,
                Some("A \"missing\" directory is created by the write."),
                self.skill_create_parent_error.as_deref(),
                pick_list(
                    self.create_parent_options(),
                    self.skill_create_parent.clone(),
                    Message::SkillCreateParentChanged,
                ),
            ),
            instructions_label,
            instructions_hint,
            text_editor(&self.skill_create_instructions)
                .placeholder("Skill instructions…")
                .on_action(Message::SkillCreateInstructionsChanged)
                .height(140)
                .font(theme.font_stack.mono)
                .size(theme.font_stack.base_size),
            row![
                create_button,
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::SkillCreateCancelled),
            ]
            .spacing(SPACING_SM),
            result_line,
        ]
        .spacing(SPACING_SM)
        .into()
    }

    /// Editable form for a selected skill's `skill.toml` (ADR-43 edit). The
    /// stable `id` stays read-only; `name` and `description` edit the manifest
    /// fields directly, while the instructions body is always written inline
    /// (replacing any `instructions_path`). `version`, `tools`, and `resources`
    /// are preserved unchanged.
    pub(super) fn ext_skill_edit_form<'a>(
        &'a self,
        theme: &'a AppTheme,
        skill: &'a SkillDescriptor,
        draft: &'a SkillEditDraft,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;
        let busy = self.skill_crud_busy;
        let tools = if skill.manifest.tools.is_empty() {
            "none".to_string()
        } else {
            skill.manifest.tools.join(", ")
        };
        let version =
            if skill.manifest.version.is_empty() { "0.0.0" } else { &skill.manifest.version };

        let save_button: Element<'a, Message> = if busy {
            button(text("Saving…").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Save").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .on_press(Message::SkillEditSaved)
                .into()
        };

        let result_line: Element<'a, Message> = match &self.skill_crud_result {
            Some(result) if result.starts_with("Error") => {
                text(result).size(11).color(palette.danger).into()
            }
            Some(_) => iced::widget::Space::new().height(0).into(),
            None => iced::widget::Space::new().height(0).into(),
        };

        let instructions_row: Element<'a, Message> = row![
            text("Instructions").size(13).color(palette.text),
            text("written inline; replaces `instructions_path`").size(11).color(palette.text_muted),
        ]
        .spacing(SPACING_XS)
        .into();

        column![
            text(format!("{} — edit", skill.id)).size(16).color(palette.text),
            ext_meta_row(theme, "ID", skill.id.as_str()),
            ext_meta_row(theme, "Version", version),
            ext_meta_row(theme, "Tools", tools),
            form_field(
                theme,
                "Name",
                false,
                Some("Blank uses the id."),
                None::<&str>,
                text_input("display name", &draft.name)
                    .on_input(Message::SkillEditNameChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Description",
                false,
                None::<&str>,
                None::<&str>,
                text_input("short description", &draft.description)
                    .on_input(Message::SkillEditDescriptionChanged)
                    .width(Length::Fill),
            ),
            instructions_row,
            text_editor(&draft.instructions)
                .placeholder("Skill instructions…")
                .on_action(Message::SkillEditInstructionsChanged)
                .height(140)
                .font(theme.font_stack.mono)
                .size(theme.font_stack.base_size),
            row![
                save_button,
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::SkillEditCancelled),
            ]
            .spacing(SPACING_SM),
            result_line,
        ]
        .spacing(SPACING_SM)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the Skills detail pane renders in read-only, edit, and
    /// delete-confirm states, and the create wizard renders, without panicking
    /// (ADR-43 create/edit/delete).
    #[test]
    fn skill_detail_renders_readonly_edit_and_delete_confirm_states() {
        let theme = AppTheme::by_name("Midnight");
        let mut state = State::from_config(&concerto_config::AppConfig::default());
        state.skills_discovered = vec![SkillDescriptor {
            id: "rust-testing".into(),
            manifest: concerto_api_types::extension::SkillManifest {
                id: "rust-testing".into(),
                name: "Rust Testing".into(),
                version: "1.0.0".into(),
                description: "Cargo verification guidance".into(),
                instructions_path: None,
                instructions: Some("Prefer cargo nextest.".into()),
                tools: vec!["cargo nextest run".into()],
                resources: Vec::new(),
            },
            instructions: "Prefer cargo nextest.".into(),
            pack_dir: std::path::PathBuf::new(),
            resource_paths: Vec::new(),
        }];
        state.ext_selected_skill = Some("rust-testing".into());

        // Read-only detail.
        let _ = state.view(&theme, false);

        // Edit mode (toggled by the "Edit" control).
        let _ = state.update(Message::SkillEditPressed("rust-testing".into()));
        let _ = state.view(&theme, false);

        // Delete-confirm state (armed, not yet confirmed).
        let _ = state.update(Message::SkillDeletePressed("rust-testing".into()));
        let _ = state.view(&theme, false);

        // Create wizard (replaces the detail).
        let _ = state.update(Message::SkillDeleteCancelled("rust-testing".into()));
        let _ = state.update(Message::SkillCreatePressed);
        let _ = state.view(&theme, false);
    }
}
