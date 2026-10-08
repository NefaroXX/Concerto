//! Skill update arms (Settings → Extensions → Skills).
//!
//! Pure relocation (NORM S32): the 26-arm Skills group of
//! [`super::state::State::update`] moves verbatim from
//! `views/settings/state.rs` into this file as a second inherent `impl State`
//! block — the same pattern as [`super::update_mcp`], [`super::shell`], and
//! [`super::state_sync`].
//!
//! The group is the master enable toggle, the per-skill allow-list toggles,
//! skill discovery, and the ADR-43 create/edit/delete wizard lifecycles. No
//! behavior, signature, call-site, or [`super::Message`] shape change: the
//! parent `update` keeps a single thin delegating arm that forwards every
//! `Message::Skill*` / `Message::Skills*` variant here.
//!
//! `update_skills` takes the full [`super::Message`] (not a sub-enum) and
//! returns `iced::Task<Message>` so the relocated arms keep their exact
//! early-return `Task` semantics (the discovery/CRUD `Task::perform`s and the
//! validation bail-outs). As in the parent, the `match` is a statement and the
//! function tail is `iced::Task::none()`. The fallback arm is unreachable
//! through the parent dispatcher and is a documented no-op.
//!
//! Master toggles and allow-list edits arm `settings_dirty` (they persist on
//! Save Settings). Discovery and CRUD results are transient view state and
//! never arm it. The state.rs skill round-trip tests stay put (they drive the
//! public `State::update` dispatch) and keep asserting the same strings.

use std::collections::HashSet;

use super::state::dedupe_lines;
use super::{Message, SkillEditDraft, State};

impl State {
    /// Handle the skill `Message` group of Settings.
    ///
    /// The parent `State::update` routes every `Message::Skill*` /
    /// `Message::Skills*` variant here. Returns the arm's `Task` unchanged; a
    /// non-skill message (never routed by the parent) is a no-op.
    pub(super) fn update_skills(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::SkillsEnabledToggled(enabled) => {
                self.settings_dirty = true;
                self.skills_enabled = enabled;
            }
            Message::SkillTogglePressed(id, on) => {
                self.settings_dirty = true;
                // Checking a skill implies intent to use skills: arm the
                // master toggle as well.
                if on {
                    self.skills_enabled = true;
                    self.ensure_skills_allow_list_materialized();
                    if !self.skills_enabled_ids.contains(&id) {
                        self.skills_enabled_ids.push(id);
                    }
                } else {
                    // Unchecking is only meaningful against an explicit
                    // allow-list: materialize it from the discovered set first
                    // so the remaining rows keep reflecting reality. An empty
                    // allow-list intentionally keeps `skills.enabled = true`
                    // (the user can re-check individual skills).
                    self.ensure_skills_allow_list_materialized();
                    self.skills_enabled_ids.retain(|existing| *existing != id);
                }
            }
            Message::SkillExpandToggled(id) => {
                // Transient view state only: expanding a preview must not arm
                // the dirty flag (nothing here persists).
                if !self.skills_expanded.remove(&id) {
                    self.skills_expanded.insert(id);
                }
            }
            Message::SkillsDiscoveryRequested => {
                return self.start_skill_discovery();
            }
            Message::SkillsDiscoveryResult(result) => {
                self.skills_loading = false;
                self.skills_loaded = true;
                match result {
                    Ok(report) => {
                        self.skills_discovered = report.descriptors;
                        self.skills_warnings = dedupe_lines(report.warnings);
                        self.skills_notes = dedupe_lines(report.notes);
                        self.skills_error = None;
                        // Keep the master-detail selection valid: default to the
                        // first pack when nothing is selected or the selected
                        // id is no longer present in the discovered set.
                        if self.ext_selected_skill.is_none()
                            || !self
                                .skills_discovered
                                .iter()
                                .any(|skill| self.ext_selected_skill.as_deref() == Some(&skill.id))
                        {
                            self.ext_selected_skill =
                                self.skills_discovered.first().map(|skill| skill.id.clone());
                        }
                        // Discovery is the ground truth for what exists on disk:
                        // drop dangling edit/delete state whose pack is gone
                        // (removed on disk, or just deleted via the UI).
                        let discovered_ids: HashSet<&str> =
                            self.skills_discovered.iter().map(|skill| skill.id.as_str()).collect();
                        if let Some(editing) = &self.skill_editing_id {
                            if !discovered_ids.contains(editing.as_str()) {
                                self.skill_editing_id = None;
                                self.skill_edit_draft = None;
                            }
                        }
                        if let Some(confirming) = &self.skill_delete_confirm {
                            if !discovered_ids.contains(confirming.as_str()) {
                                self.skill_delete_confirm = None;
                            }
                        }
                    }
                    Err(error) => {
                        self.skills_error = Some(error);
                        self.skills_warnings = Vec::new();
                        self.skills_notes = Vec::new();
                    }
                }
            }

            // ADR-43 — skill pack CRUD (create wizard / edit / delete).
            // All of these are transient wizard/confirm/result messages: they
            // never arm `settings_dirty`, and pack files only land on disk via
            // the skills crate's create/update/delete operations inside the
            // spawned task.
            Message::SkillCreatePressed => {
                // Opening the wizard resets the draft and drops the previous
                // CRUD result (it described a different skill's operation).
                self.skill_crud_result = None;
                self.skill_create_id_error = None;
                self.skill_create_parent_error = None;
                self.skill_create_id = String::new();
                self.skill_create_name = String::new();
                self.skill_create_version = "0.1.0".to_string();
                self.skill_create_description = String::new();
                self.skill_create_instructions = iced::widget::text_editor::Content::new();
                self.skill_create_parent = self.create_parent_options().into_iter().next();
                self.skill_create_open = true;
            }
            Message::SkillCreateIdChanged(id) => {
                self.skill_create_id = id.clone();
                self.skill_create_id_error = Self::skill_id_error(&id);
                self.skill_crud_result = None;
            }
            Message::SkillCreateNameChanged(name) => self.skill_create_name = name,
            Message::SkillCreateVersionChanged(version) => self.skill_create_version = version,
            Message::SkillCreateDescriptionChanged(description) => {
                self.skill_create_description = description
            }
            Message::SkillCreateInstructionsChanged(action) => {
                self.skill_create_instructions.perform(action);
            }
            Message::SkillCreateParentChanged(parent) => {
                self.skill_create_parent = Some(parent);
                self.skill_create_parent_error = None;
                self.skill_crud_result = None;
            }
            Message::SkillCreateConfirmed => {
                // Validate id and parent before touching the filesystem.
                if let Some(error) = Self::skill_id_error(&self.skill_create_id) {
                    self.skill_create_id_error = Some(error);
                    return iced::Task::none();
                }
                let Some(parent_option) = self.skill_create_parent.clone() else {
                    self.skill_create_parent_error = Some("Pick a parent directory.".to_string());
                    return iced::Task::none();
                };
                self.skill_create_id_error = None;
                self.skill_create_parent_error = None;
                self.skill_crud_busy = true;
                let parent = parent_option.raw;
                let id = self.skill_create_id.trim().to_string();
                let name = {
                    let trimmed = self.skill_create_name.trim();
                    if trimmed.is_empty() {
                        id.clone()
                    } else {
                        trimmed.to_string()
                    }
                };
                let version = {
                    let trimmed = self.skill_create_version.trim();
                    if trimmed.is_empty() {
                        "0.1.0".to_string()
                    } else {
                        trimmed.to_string()
                    }
                };
                let description = self.skill_create_description.trim().to_string();
                let instructions = self.skill_create_instructions.text();
                return iced::Task::perform(
                    async move {
                        super::helpers::create_skill_pack(
                            parent,
                            id,
                            name,
                            version,
                            description,
                            instructions,
                        )
                    },
                    Message::SkillCreateResult,
                );
            }
            Message::SkillCreateCancelled => {
                self.skill_create_open = false;
                self.skill_crud_result = None;
                self.skill_create_id_error = None;
                self.skill_create_parent_error = None;
            }
            Message::SkillCreateResult(result) => {
                self.skill_crud_busy = false;
                match result {
                    Ok(outcome) => {
                        self.skill_crud_result = Some(outcome);
                        self.skill_create_open = false;
                        self.skill_create_id_error = None;
                        self.skill_create_parent_error = None;
                        // The pack landed on disk; refresh so it shows up in the
                        // discovered list (and becomes selectable).
                        return self.start_skill_discovery();
                    }
                    Err(error) => {
                        // Stay in the wizard so the user can correct the draft
                        // (e.g. an id that already exists on disk).
                        self.skill_crud_result = Some(format!("Error: {error}"));
                    }
                }
            }
            Message::SkillEditPressed(id) => {
                let Some(skill) = self.skills_discovered.iter().find(|skill| skill.id == id) else {
                    return iced::Task::none();
                };
                self.skill_crud_result = None;
                self.skill_editing_id = Some(id);
                self.skill_edit_draft = Some(SkillEditDraft {
                    name: skill.manifest.name.clone(),
                    description: skill.manifest.description.clone(),
                    instructions: iced::widget::text_editor::Content::with_text(
                        &skill.instructions,
                    ),
                });
            }
            Message::SkillEditNameChanged(name) => {
                if let Some(draft) = &mut self.skill_edit_draft {
                    draft.name = name;
                }
                self.skill_crud_result = None;
            }
            Message::SkillEditDescriptionChanged(description) => {
                if let Some(draft) = &mut self.skill_edit_draft {
                    draft.description = description;
                }
                self.skill_crud_result = None;
            }
            Message::SkillEditInstructionsChanged(action) => {
                if let Some(draft) = &mut self.skill_edit_draft {
                    draft.instructions.perform(action);
                }
                self.skill_crud_result = None;
            }
            Message::SkillEditSaved => {
                let Some(id) = self.skill_editing_id.clone() else {
                    return iced::Task::none();
                };
                let Some(skill) =
                    self.skills_discovered.iter().find(|skill| skill.id == id).cloned()
                else {
                    // The pack vanished between discovery and now; leave edit
                    // mode instead of writing to a gone directory.
                    self.skill_editing_id = None;
                    self.skill_edit_draft = None;
                    return iced::Task::none();
                };
                let Some(draft) = &self.skill_edit_draft else {
                    return iced::Task::none();
                };
                self.skill_crud_busy = true;
                let pack_dir = skill.pack_dir.to_string_lossy().into_owned();
                let name = {
                    let trimmed = draft.name.trim();
                    if trimmed.is_empty() {
                        skill.id.clone()
                    } else {
                        trimmed.to_string()
                    }
                };
                let manifest = concerto_api_types::extension::SkillManifest {
                    id: skill.id.clone(),
                    name,
                    version: skill.manifest.version.clone(),
                    description: draft.description.trim().to_string(),
                    instructions_path: None,
                    instructions: Some(draft.instructions.text()),
                    tools: skill.manifest.tools.clone(),
                    resources: skill.manifest.resources.clone(),
                };
                return iced::Task::perform(
                    async move { super::helpers::update_skill_pack(pack_dir, manifest) },
                    Message::SkillEditResult,
                );
            }
            Message::SkillEditCancelled => {
                self.skill_editing_id = None;
                self.skill_edit_draft = None;
                self.skill_crud_result = None;
            }
            Message::SkillEditResult(result) => {
                self.skill_crud_busy = false;
                match result {
                    Ok(outcome) => {
                        self.skill_crud_result = Some(outcome);
                        self.skill_editing_id = None;
                        self.skill_edit_draft = None;
                        return self.start_skill_discovery();
                    }
                    Err(error) => {
                        // Stay in edit mode so the user can fix the draft (e.g.
                        // a read-only pack directory).
                        self.skill_crud_result = Some(format!("Error: {error}"));
                    }
                }
            }
            Message::SkillDeletePressed(id) => {
                // First press only arms the confirm prompt (destructive action;
                // same explicit-confirm rule as providers and MCP servers).
                self.skill_delete_confirm = Some(id);
                self.skill_crud_result = None;
            }
            Message::SkillDeleteCancelled(id) => {
                if self.skill_delete_confirm.as_deref() == Some(id.as_str()) {
                    self.skill_delete_confirm = None;
                }
            }
            Message::SkillDeleteConfirmed(id) => {
                // Clear the prompt immediately: the task below is the one
                // destructive step, and the confirm row must not linger.
                self.skill_delete_confirm = None;
                let Some(pack_dir) = self
                    .skills_discovered
                    .iter()
                    .find(|skill| skill.id == id)
                    .map(|skill| skill.pack_dir.to_string_lossy().into_owned())
                else {
                    return iced::Task::none();
                };
                self.skill_crud_busy = true;
                return iced::Task::perform(
                    async move { super::helpers::delete_skill_pack(pack_dir) },
                    Message::SkillDeleteResult,
                );
            }
            Message::SkillDeleteResult(result) => {
                self.skill_crud_busy = false;
                match result {
                    Ok(outcome) => {
                        self.skill_crud_result = Some(outcome);
                        // The pack is gone from disk; refresh so the list no
                        // longer shows it. If it was selected, discovery
                        // re-defaults the selection to the first remaining pack.
                        return self.start_skill_discovery();
                    }
                    Err(error) => {
                        self.skill_crud_result = Some(format!("Error: {error}"));
                    }
                }
            }
            _ => {}
        }
        iced::Task::none()
    }
}
