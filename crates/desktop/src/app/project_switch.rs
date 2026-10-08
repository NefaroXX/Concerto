//! Project-switch cluster of `App::update()` — one cluster extracted from
//! `app.rs` (NORM slice S31).
//!
//! This module owns the root update match's project-dir / root-consent /
//! project-tree arms: the dir-picker open/input/cancel/apply flow, the
//! ADR-44 §4 consent gate (`RootConsentAllow`/`RootConsentDeny`), the
//! sidebar tree's expand + lazy session load + session click, and the
//! same-project `Chat::SelectSession` re-dispatch. The bodies moved
//! verbatim from `app.rs` — at the same 12-space arm indent, so each copy
//! is line-for-line — and the only structural edit is the group becoming
//! one `pub(super)` method that takes the full [`Message`] and re-matches
//! it (the same pattern as `simple_updates::update_cancel_and_ticks`), so
//! every arm keeps its exact early-return `Task` semantics. The fallback
//! arm in the method is unreachable through the parent dispatcher and is a
//! documented no-op. The parent `update` keeps one thin delegating arm;
//! the project-dir / consent / tree tests in `app.rs`'s `mod tests` drive
//! `update()` unchanged.
//!
//! The `switch_project_dir` hub helper moved with the cluster: all three
//! of its call sites are the arms above. `rebuild_project_tree` and
//! `load_sessions_for_project` stay in `app.rs` — they are also called by
//! `App::new`'s boot, and `load_sessions_for_project` by the post-run
//! refresh, so they are not only/primarily this cluster's helpers.

use super::*;

impl App {
    /// Project-dir picker, root-consent gate, and project-tree arms (NORM
    /// S31).
    ///
    /// The parent routes `OpenProjectDirPicker`, `ProjectDirInputChanged`,
    /// `ProjectDirCancel`, `ProjectDirApply`, `RootConsentAllow`,
    /// `RootConsentDeny`, `ToggleProjectExpanded`, `ProjectSessionsLoaded`,
    /// and `TreeSessionClicked` here. The picker arms toggle the modal and
    /// its text input; `ProjectDirApply` / `RootConsentAllow` run the
    /// ADR-44 §4 consent gate and then [`Self::switch_project_dir`];
    /// `RootConsentDeny` aborts the deferred switch; the tree arms expand a
    /// node (lazily loading its sessions), cache a loaded session list, and
    /// resume a clicked session (re-dispatching `Chat::SelectSession` for
    /// the same-project case). Returns each arm's `Task` unchanged; a
    /// message the parent never routes here is a documented no-op.
    pub(super) fn update_project_switch(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::OpenProjectDirPicker => {
                self.show_dir_picker = true;
                self.project_dir_input = self.project_dir.to_string_lossy().to_string();
                iced::Task::none()
            }
            Message::ProjectDirInputChanged(s) => {
                self.project_dir_input = s;
                iced::Task::none()
            }
            Message::ProjectDirCancel => {
                self.show_dir_picker = false;
                iced::Task::none()
            }
            Message::ProjectDirApply => {
                if self.run_status != RunStatus::Idle {
                    self.toasts.push(
                        ToastLevel::Info,
                        "Cancel the running task before switching projects.".to_string(),
                    );
                    return iced::Task::none();
                }
                let candidate = std::path::PathBuf::from(self.project_dir_input.trim());
                if candidate.is_dir() {
                    // ADR-44 §4: gate out-of-root switches behind the consent
                    // modal. Re-selecting the current project is a no-op and
                    // never gates — nothing new is exposed.
                    let canonical = concerto_core::helpers::canonical_project_path(&candidate);
                    let current = concerto_core::helpers::canonical_project_path(&self.project_dir);
                    if canonical != current
                        && root_consent::needs_consent(&canonical, &self.effective_roots)
                    {
                        self.pending_root_consent = Some(canonical);
                        return iced::Task::none();
                    }
                    return self.switch_project_dir(&candidate);
                }
                // If not a directory, keep the modal open unchanged.
                iced::Task::none()
            }
            Message::RootConsentAllow => {
                let Some(canonical) = self.pending_root_consent.take() else {
                    return iced::Task::none();
                };
                // The user allowed the canonical dir for this process: record
                // it in the effective allowlist, then apply the switch.
                if !self.effective_roots.contains(&canonical) {
                    self.effective_roots.push(canonical.clone());
                }
                self.switch_project_dir(&canonical)
            }
            Message::RootConsentDeny => {
                // Abort the deferred switch cleanly: no project change, no
                // error spam. The dir picker (if open behind the gate) stays
                // open so the user can correct the path or cancel.
                self.pending_root_consent = None;
                self.pending_tree_session = None;
                iced::Task::none()
            }
            Message::ToggleProjectExpanded(path) => {
                let Some(node) = self.project_tree.iter_mut().find(|n| n.path == path) else {
                    return iced::Task::none();
                };
                if node.expanded {
                    node.expanded = false;
                    return iced::Task::none();
                }
                node.expanded = true;
                // Lazy load: sessions are fetched once on the first expand and
                // cached in the node afterwards.
                if node.sessions.is_none() {
                    self.load_sessions_for_project(path)
                } else {
                    iced::Task::none()
                }
            }
            Message::ProjectSessionsLoaded { path, sessions } => {
                if let Some(node) = self.project_tree.iter_mut().find(|n| n.path == path) {
                    node.sessions = Some(sessions);
                }
                iced::Task::none()
            }
            Message::TreeSessionClicked { project, session_id } => {
                let canonical = concerto_core::helpers::canonical_project_path(&project);
                let current = concerto_core::helpers::canonical_project_path(&self.project_dir);
                if canonical == current {
                    // Same project: resume the session in place, no switch.
                    return self
                        .update(Message::Chat(views::chat::Message::SelectSession(session_id)));
                }
                if self.run_status != RunStatus::Idle {
                    self.toasts.push(
                        ToastLevel::Info,
                        "Cancel the running task before switching projects.".to_string(),
                    );
                    return iced::Task::none();
                }
                // ADR-44 §4: out-of-root switches go through the consent gate.
                // Remember the session so the deferred switch resumes it.
                if root_consent::needs_consent(&canonical, &self.effective_roots) {
                    self.pending_root_consent = Some(canonical);
                    self.pending_tree_session = Some(session_id);
                    return iced::Task::none();
                }
                self.pending_tree_session = Some(session_id);
                self.switch_project_dir(&project)
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// Switch the active project folder to `path` (a directory) and reset all
    /// project-scoped state for the new folder.
    ///
    /// Shared by [`Message::ProjectDirApply`] (in-root or already-consented
    /// switches) and [`Message::RootConsentAllow`] (the deferred out-of-root
    /// switch), so Allow continues the exact flow that would have run without
    /// the gate.
    pub(super) fn switch_project_dir(&mut self, path: &Path) -> iced::Task<Message> {
        let mut registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
        self.project_dir = registry.select(path).unwrap_or_else(|_| path.to_path_buf());
        if let Err(error) = registry.save() {
            tracing::warn!(%error, "failed to persist project registry");
        }
        self.show_dir_picker = false;
        // Collapse the config reload + full re-derivation onto the single
        // shared helper (ADR-57 §4). Project re-select also re-arms the
        // config-watch path set automatically: the subscription's identity is
        // the project dir, so iced recreates the watcher when it changes.
        self.reconcile_config_from_reload();
        // Rebind the session handler to the new folder on next run.
        *self.session_manager.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Cancel current memory lifecycle and clear it
        if let Some(prev) = self.memory_services.lock().unwrap_or_else(|e| e.into_inner()).take() {
            prev.cancel.cancel();
        }
        self.memory = views::memory::State::new();
        self.sync_memory_configuration();
        self.tool_log = views::tool_log::State::new();
        self.vfs = Arc::new(Mutex::new(VirtualFs::new()));
        // A project switch starts a blank session. The folder's
        // earlier sessions remain available in Recent sessions.
        self.active_session_id = None;
        self.resume_checkpoint_json = None;
        self.reset_spend_state();
        self.agent_graph = views::agent_graph::State::new();
        self.chat = views::chat::State::new();
        self.seed_muted_agents();
        let terminal = self
            .terminal
            .set_project_dir(self.project_dir.clone(), &self.current_theme)
            .map(Message::Terminal);
        // Rebuild the tree around the new active project (its node is expanded
        // by default) and reload its sessions. If this switch came from a tree
        // session click, resume that session once the switch is applied.
        self.rebuild_project_tree();
        let mut tasks = vec![
            terminal,
            self.load_sessions_for_project(self.project_dir.clone()),
            self.load_git_summary(),
        ];
        if let Some(session_id) = self.pending_tree_session.take() {
            tasks.push(self.select_session(session_id));
        }
        iced::Task::batch(tasks)
    }
}
