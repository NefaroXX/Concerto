//! The `AgentRunCompleted` arm for [`App`] — NORM slice S43.
//!
//! This module owns the single deepest-coupled arm of the root `update`
//! match: the run-settled teardown that runs exactly once at every agent-run
//! boundary. It resets the run status/stage, flushes any deferred memory
//! teardown, closes the open thinking phases, settles the chat tool calls and
//! the agent graph against the outcome, stores (or clears) the resume
//! checkpoint, persists the transcript and the active agent graph, reloads the
//! VFS diff, and returns the session-list + git-summary refresh batch.
//!
//! The body moved verbatim from `app.rs` — at the same 12-space arm indent, so
//! the copy is line-for-line with its origin — and the only structural edit is
//! the arm becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as the sibling `simple_updates` /
//! `project_switch` / `delegated_updates` / `event_model_updates` /
//! `view_dispatch` / `settings_updates` / `navigation_updates` /
//! `update_routing` groups), so the arm keeps its exact `Task` semantics. The
//! parent `update` keeps one thin delegating arm; the fallback arm here is
//! unreachable through the parent dispatcher and is a documented no-op. No
//! `Message` / `App` shape change and no behavior change: every helper the arm
//! calls stays in `app.rs`, and the tests in `app.rs`'s `mod tests` stay put
//! and drive `update()` unchanged.

use super::*;

impl App {
    /// The `AgentRunCompleted` arm (NORM S43): the run-settled teardown.
    ///
    /// The parent routes the `AgentRunCompleted` message here. It applies the
    /// outcome (`Ok` settles the run as completed/cancelled; `Err` settles it
    /// as cancelled/failed and surfaces the failure to the user), stores the
    /// resume checkpoint, persists the transcript + active agent graph, and
    /// reloads the sessions and git summary. Returns the arm's `Task`
    /// unchanged; a message the parent never routes here is a documented
    /// no-op.
    pub(super) fn update_run_completed(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::AgentRunCompleted(session_id, res) => {
                self.run_status = RunStatus::Idle;
                self.note_run_settled();
                // ADR-57 §3a: memory teardown may have been deferred while the
                // run was active (a config edit that disables memory is not
                // hot-applied mid-run); the run is over, so complete it now.
                self.sync_memory_configuration();
                // The run is over: drop the stage chip regardless of outcome
                // (Ok or Err both pass through here).
                self.run_stage = None;
                // True run boundary: no further thinking can arrive, so close
                // every open thinking phase (navigation's finalize_streaming
                // deliberately leaves them open).
                self.chat.finalize_run();
                // Keep the newly-created session active even when the run
                // fails. A retry then continues the same conversation instead
                // of silently creating another session and losing context.
                if let Some(session_id) = session_id {
                    self.active_session_id = Some(session_id);
                }
                match *res {
                    Ok(output) => {
                        let completed =
                            output.completion_status == AgentCompletionStatus::Completed;
                        self.chat.settle_running_tool_calls(if completed {
                            views::chat::ToolCallStatus::Completed
                        } else {
                            views::chat::ToolCallStatus::Cancelled
                        });
                        self.agent_graph.settle_incomplete(if completed {
                            NodeState::Completed
                        } else {
                            NodeState::Cancelled
                        });
                        self.active_session_id = Some(output.session_id);
                        let final_message = format_run_summary(&output);
                        let _ = self.chat.update(views::chat::Message::AddAssistant(final_message));
                        self.chat.set_run_completion(
                            self.multi_agent,
                            completed,
                            output.files_modified.iter().map(ToString::to_string).collect(),
                            output.project_root.as_ref().map(ToString::to_string),
                        );
                        self.load_diff_from_vfs();
                        // Store checkpoint for potential resume.
                        self.resume_checkpoint_json = output.checkpoint_json.clone();
                    }
                    Err(failure) => {
                        // Clear any stored checkpoint so a failed resume does
                        // not poison subsequent messages (Finding 2 / #65).
                        self.resume_checkpoint_json = None;

                        let terminal_state = if failure.code == "TASK_CANCELLED" {
                            NodeState::Cancelled
                        } else {
                            NodeState::Failed
                        };
                        self.chat.settle_running_tool_calls(if failure.code == "TASK_CANCELLED" {
                            views::chat::ToolCallStatus::Cancelled
                        } else {
                            views::chat::ToolCallStatus::Failed
                        });
                        self.agent_graph.settle_incomplete(terminal_state);
                        match failure.audience {
                            FailureAudience::User => {
                                let _ = self.chat.update(views::chat::Message::AddAssistant(
                                    failure.user_message,
                                ));
                            }
                            FailureAudience::Developer => {
                                tracing::error!(
                                    code = %failure.code,
                                    details = %failure.dev_details
                                );

                                let error_message = format!(
                                    "{}\n\nError code: {}",
                                    failure.user_message, failure.code
                                );
                                let _ = self
                                    .chat
                                    .update(views::chat::Message::AddAssistant(error_message));
                            }
                            _ => {}
                        }
                    }
                }
                // Persist the transcript so the on-screen conversation survives a
                // restart. Best-effort: a write failure must never break the UI.
                let project_dir = self.project_dir.clone();
                if let Some(session_id) = self.active_session_id {
                    let session_id = session_id.to_string();
                    let _ = self.chat.save_to(&transcript_path(&project_dir, &session_id));
                }
                self.persist_active_agent_graph();
                iced::Task::batch(vec![
                    self.load_sessions_for_project(self.project_dir.clone()),
                    self.load_git_summary(),
                ])
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
