//! Agent-run settlement for [`App`].
//!
//! Clear unconfirmed continuation views when a fresh run settles without any
//! specialist lifecycle events, then apply the existing outcome, transcript,
//! checkpoint, graph persistence and session-refresh handling.

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
            Message::AgentRunCompleted(completion) => {
                if !self.conversation.settle(&completion) {
                    return iced::Task::none();
                }
                let display_message = completion.display_message();
                let ConversationCompletion { result, undelivered, reply_only, .. } = *completion;
                if !undelivered.is_empty() {
                    let mut recovered = undelivered.join("\n\n");
                    let draft = self.chat.input();
                    if !draft.trim().is_empty() {
                        recovered.push_str("\n\n");
                        recovered.push_str(&draft);
                    }
                    let _ = self.chat.update(views::chat::Message::InputChanged(recovered));
                    self.chat.add_error("Work stopped before reading your latest messages. They are restored in the composer.".into());
                }
                // A discarded checkpoint can settle without specialist events
                // (e.g. an answer-only task); clear its old presentation here.
                self.reset_pending_task_views();
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
                match result {
                    Ok(output) => {
                        let completed =
                            output.completion_status == AgentCompletionStatus::Completed;
                        self.chat.settle_running_tool_calls(if completed {
                            views::chat::ToolCallStatus::Completed
                        } else {
                            views::chat::ToolCallStatus::Cancelled
                        });
                        if !reply_only {
                            self.agent_graph.settle_incomplete(if completed {
                                NodeState::Completed
                            } else {
                                NodeState::Cancelled
                            });
                        }
                        let final_message = display_message;
                        let _ = self.chat.update(views::chat::Message::AddAssistant(final_message));
                        if !reply_only {
                            self.chat.set_run_completion(
                                self.multi_agent,
                                completed,
                                output.files_modified.iter().map(ToString::to_string).collect(),
                                output.project_root.as_ref().map(ToString::to_string),
                            );
                        }
                        self.load_diff_from_vfs();
                    }
                    Err(failure) => {
                        // The shared controller reloads durable recovery state after errors.

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
                if let Some(session_id) = self.conversation.session_id {
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
