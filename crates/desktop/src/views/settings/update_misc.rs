//! Remaining Settings update arms (theme/display, relationships, save/section
//! navigation, Extensions tab/item selection, project context).
//!
//! Pure relocation (NORM S36): the last five non-shell arm groups of
//! [`super::state::State::update`] move verbatim from `views/settings/state.rs`
//! into this file as five additional inherent `impl State` methods — the same
//! pattern as [`super::update_mcp`], [`super::update_skills`],
//! [`super::update_providers`], [`super::update_plugins`],
//! [`super::update_policy`], [`super::shell`], and [`super::state_sync`].
//!
//! Unlike the earlier slices these groups were SCATTERED through the pre-refactor
//! `update` body rather than forming one contiguous block, so they share this
//! file and the parent keeps one thin delegating OR-pattern arm per group:
//!
//! * [`State::update_theme`] — the 4-arm theme/display knobs
//!   (`ThemeSelected`, `FontSizeChanged`, `ReducedMotionToggled`,
//!   `ScanlineOverlayToggled`).
//! * [`State::update_relationships`] — the 6-arm relationship builder
//!   (`Relationship*`), including its inline validation.
//! * [`State::update_navigation`] — save and section navigation
//!   (`SaveSettings`, `ToggleSection`, `JumpToSection`).
//! * [`State::update_extension_tab`] — the Extensions tab / master-list
//!   selection (`ExtensionTabSelected`, `ExtensionItemSelected`).
//! * [`State::update_project_context`] — the ADR-70 project-context toggles
//!   (`ProjectContext*Toggled`).
//!
//! The five variant sets are disjoint, so a single combined delegating arm would
//! also be correct; one arm per group was chosen so each thin arm names the same
//! domain as the method it forwards to and the relocation stays easy to review.
//!
//! No behavior, signature, call-site, or [`super::Message`] shape change: each
//! helper takes the full [`super::Message`] (not a sub-enum) and returns
//! `iced::Task<Message>`, preserving every early-return `Task` (the relationship
//! self-reference bail-out, `JumpToSection`'s scroll, the lazy plugin-list
//! refresh, and the project-context default-cascade). As in the parent, each
//! `match` is a statement and each function tail is `iced::Task::none()`. The
//! fallback arm is unreachable through the parent dispatcher and is a documented
//! no-op.
//!
//! Test coverage for these arms lives in the `views/settings/helpers.rs` and
//! `views/settings/state.rs` test modules and drives the public
//! [`super::state::State::update`] dispatch, so it stays put.

use concerto_config::AgentRelationshipConfig;

use super::state::State;
use super::{ExtensionTab, Message};

impl State {
    /// Handle the theme / display `Message` group of Settings.
    ///
    /// The parent `State::update` routes the four `ThemeSelected`,
    /// `FontSizeChanged`, `ReducedMotionToggled`, and `ScanlineOverlayToggled`
    /// variants here. Returns the arm's `Task` unchanged; a non-theme message
    /// (never routed by the parent) is a no-op.
    pub(super) fn update_theme(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ThemeSelected(name) => self.selected_theme = name,
            Message::FontSizeChanged(size) => self.font_size = size.clamp(12.0, 20.0),
            Message::ReducedMotionToggled(reduced) => {
                self.settings_dirty = true;
                self.reduced_motion = reduced;
            }
            Message::ScanlineOverlayToggled(enabled) => {
                self.settings_dirty = true;
                self.scanline_overlay_enabled = enabled;
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// Handle the relationship builder `Message` group of Settings.
    ///
    /// The parent `State::update` routes every `Relationship*` variant here.
    /// Relationships are persisted on explicit Save Settings. Returns the arm's
    /// `Task` unchanged; a non-relationship message (never routed by the
    /// parent) is a no-op.
    pub(super) fn update_relationships(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::RelationshipFromChanged(role) => {
                self.settings_dirty = true;
                self.new_relationship_from = role;
                self.relationship_warning = None;
            }
            Message::RelationshipToChanged(role) => {
                self.settings_dirty = true;
                self.new_relationship_to = role;
                self.relationship_warning = None;
            }
            Message::RelationshipTypeChanged(relationship) => {
                self.settings_dirty = true;
                self.new_relationship_type = relationship;
                self.relationship_warning = None;
            }
            Message::RelationshipCyclesChanged(value) => {
                self.settings_dirty = true;
                self.new_relationship_cycles = value;
                self.relationship_warning = None;
            }
            Message::RelationshipAdded => {
                self.settings_dirty = true;
                self.relationship_dirty = true;
                // Inline validation: surface problems instead of silently
                // dropping or overwriting rules.
                if self.new_relationship_from == self.new_relationship_to {
                    self.relationship_warning =
                        Some("An agent cannot have a relationship with itself.".into());
                    return iced::Task::none();
                }
                let max_cycles = self
                    .new_relationship_cycles
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|cycles| *cycles > 0);
                let rule = AgentRelationshipConfig {
                    from: self.new_relationship_from.into(),
                    to: self.new_relationship_to.into(),
                    relationship: self.new_relationship_type.into(),
                    max_cycles,
                };
                let duplicate = self
                    .relationship_rules
                    .iter()
                    .any(|existing| existing.from == rule.from && existing.to == rule.to);
                if duplicate {
                    self.relationship_warning = Some(format!(
                        "A relationship from '{}' to '{}' already exists; the new rule replaces it.",
                        rule.from, rule.to
                    ));
                } else {
                    self.relationship_warning = None;
                }
                if let Some(existing) = self
                    .relationship_rules
                    .iter_mut()
                    .find(|existing| existing.from == rule.from && existing.to == rule.to)
                {
                    *existing = rule;
                } else {
                    self.relationship_rules.push(rule);
                }
            }
            Message::RelationshipRemoved(index) => {
                self.settings_dirty = true;
                self.relationship_dirty = true;
                if index < self.relationship_rules.len() {
                    self.relationship_rules.remove(index);
                }
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// Handle the save / section-navigation `Message` group of Settings.
    ///
    /// The parent `State::update` routes `SaveSettings`, `ToggleSection`, and
    /// `JumpToSection` here. Returns the arm's `Task` unchanged; a non-navigation
    /// message (never routed by the parent) is a no-op.
    pub(super) fn update_navigation(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::SaveSettings => {
                self.settings_saved_notice = true;
                self.settings_dirty = false;
                self.relationship_dirty = false;
            }
            Message::ToggleSection(id) => {
                if !self.collapsed_sections.remove(&id) {
                    self.collapsed_sections.insert(id);
                }
            }
            // Sidebar navigation: always expand the target (never fold it) and
            // scroll the main column to its header.
            Message::JumpToSection(id) => {
                self.collapsed_sections.remove(&id);
                return Self::scroll_to_section(id);
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// Handle the Extensions tab / master-list selection `Message` group.
    ///
    /// The parent `State::update` routes `ExtensionTabSelected` and
    /// `ExtensionItemSelected` here. Tab switching and list selection are
    /// transient view state: they never arm the dirty flag and are never
    /// persisted. Returns the arm's `Task` unchanged; a non-extension message
    /// (never routed by the parent) is a no-op.
    pub(super) fn update_extension_tab(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ExtensionTabSelected(tab) => {
                self.active_extension_tab = tab;
                if tab == ExtensionTab::Plugins {
                    // The installed-plugin list is scanned once, lazily, the
                    // first time the tab is opened (mirrors skill discovery).
                    if !self.plugins_loaded && !self.plugins_loading {
                        return self.start_plugin_list_refresh();
                    }
                }
            }
            Message::ExtensionItemSelected(tab, id) => {
                match tab {
                    ExtensionTab::Skills => {
                        // A CRUD outcome belongs to the previously selected
                        // skill; drop it so the next selection never shows a
                        // stale result line.
                        self.skill_crud_result = None;
                        self.ext_selected_skill = Some(id);
                    }
                    ExtensionTab::Mcp => self.ext_selected_mcp = Some(id),
                    ExtensionTab::Plugins => {
                        // A revoke or delete outcome belongs to the previously
                        // selected plugin; drop it so the next selection never
                        // shows a stale result line.
                        self.plugin_revoke_result = None;
                        self.plugin_install_result = None;
                        self.plugin_delete_confirm = None;
                        self.ext_selected_plugin = Some(id);
                    }
                    // The project-context tab has no item list.
                    ExtensionTab::ProjectContext => {}
                }
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// Handle the ADR-70 project-context `Message` group of Settings.
    ///
    /// The parent `State::update` routes both `ProjectContext*Toggled` variants
    /// here. Persisted on Save Settings; each edit explicitly arms
    /// `project_context_dirty` so a plain save never publishes the startup
    /// snapshot. Returns the arm's `Task` unchanged; a non-project-context
    /// message (never routed by the parent) is a no-op.
    pub(super) fn update_project_context(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ProjectContextEnabledToggled(enabled) => {
                self.settings_dirty = true;
                self.project_context_dirty = true;
                self.project_context_enabled = enabled;
                // Disabling the feature also quiets the nudge: ADR-70 gates the
                // advisory on the whole feature being active, so leaving the
                // nudge on after a disable would be dead config.
                if !enabled {
                    self.project_context_auto_update_agents_md = false;
                }
            }
            Message::ProjectContextNudgeToggled(enabled) => {
                self.settings_dirty = true;
                self.project_context_dirty = true;
                self.project_context_auto_update_agents_md = enabled;
            }
            _ => {}
        }
        iced::Task::none()
    }
}
