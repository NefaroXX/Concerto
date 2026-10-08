//! Approval-sink wiring for [`App`] — one cluster extracted from `app.rs`
//! (NORM slice 22-A).
//!
//! This module owns [`DesktopApprovalSink`] (the desktop half of the
//! `ApprovalSink` trait: capability dialogs, acks, intent/plan confirmations,
//! session-wide auto-approve), the [`desktop_approval_sink`] ctor, and the
//! pure [`format_run_summary`] formatter. The body moved verbatim from
//! `app.rs`; the only edits are the `pub(super)` annotations on the struct,
//! its fields, and the ctor (module-private to `app.rs` before the move, so
//! effective visibility is unchanged). The approval tests stay in `app.rs`'s
//! `mod tests`, reaching these names through the parent's imports.

use super::*;

#[derive(Clone)]
pub(super) struct DesktopApprovalSink {
    pub(super) cap_pending: crate::widgets::capability_dialog::SharedPending,
    pub(super) pending_ack: crate::widgets::capability_dialog::SharedPendingAck,
    pub(super) pending_intent: crate::widgets::capability_dialog::SharedPendingIntent,
    pub(super) pending_plan: crate::widgets::capability_dialog::SharedPendingPlan,
    /// Session-wide auto-approve flag, mirroring the CLI sink's semantics:
    /// once the user chooses "approve all for session" (or the dialog's
    /// "Always allow"), every subsequent request is approved without a prompt
    /// until the next run. A plain single grant never flips this flag.
    pub(super) auto_approve: Arc<AtomicBool>,
    pub(super) bus: EventBus,
}

#[async_trait::async_trait]
impl ApprovalSink for DesktopApprovalSink {
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        // Fast path: auto-approve if enabled (mirrors the CLI sink).
        if self.auto_approve.load(Ordering::Relaxed) {
            return ApprovalDecision::Approve;
        }

        use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
        use concerto_plugins::capability::GrantDecision;

        let detail = format!("{:?}", action.input);
        let caps = match action.tool_name {
            "write_file" => {
                vec![CapabilityRequest::FilesystemWrite { globs: vec![detail] }]
            }
            "read_file" => {
                vec![CapabilityRequest::FilesystemRead { globs: vec![detail] }]
            }
            "shell_execute" => {
                vec![CapabilityRequest::ShellExecute { allowlist: vec![detail] }]
            }
            "network_access" => {
                vec![CapabilityRequest::NetworkOutbound { domains: vec![detail] }]
            }
            other => {
                vec![CapabilityRequest::Other { description: format!("Tool: {}", other) }]
            }
        };

        let name = format!("{} request", action.tool_name);

        // Coalesce identical pending requests (same tool + input) onto one
        // dialog instead of stacking duplicates: a retry/resume re-attaches to
        // the EXISTING decision slot. The `watch` receiver is cloneable.
        let key = format!("{}::{}", action.tool_name, action.input);
        let (rx, queued_new) = {
            let mut guard = self.cap_pending.lock().unwrap_or_else(|e| e.into_inner());
            match guard.iter().find(|pending| pending.key == key) {
                Some(existing) => (existing.receiver.clone(), false),
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    guard.push_back(crate::widgets::capability_dialog::PendingApproval {
                        plugin: PluginManifest {
                            name: name.clone(),
                            description: "Policy action".into(),
                            version: "1.0".into(),
                            id: name,
                            abi_version: 1,
                            capabilities_required: caps.clone(),
                            provides: Vec::new(),
                        },
                        capabilities: caps,
                        key,
                        sender: tx,
                        receiver: rx.clone(),
                    });
                    (rx, true)
                }
            }
        };

        // Surface a NEWLY-queued request to the UI. `cap_pending` is mutated
        // from this async task, so without an explicit event the Iced view
        // would not re-render and the dialog would stay invisible while the
        // agent blocks waiting on it. Publishing guarantees a redraw that shows
        // the dialog; a coalesced request reuses the already-visible dialog.
        if queued_new {
            let _ = self.bus.publish_for_session(
                action.session_id,
                action.correlation_id,
                concerto_core::event::EventKind::ApprovalRequested {
                    tool_name: action.tool_name.to_string(),
                    timeout_secs: 0,
                },
            );
        }

        match crate::widgets::capability_dialog::await_decision(rx).await {
            Some(decisions) => {
                // Every requested capability must be granted for the action to
                // proceed (a single Denied button denies the whole request).
                let all_granted = decisions.iter().all(|d| {
                    matches!(d, GrantDecision::Granted | GrantDecision::GrantedPersistent)
                });
                if !all_granted {
                    return ApprovalDecision::Deny;
                }
                // The dialog resolves every capability with the same button, so
                // a session-wide "Always allow" grant is uniform across the
                // list. Flip the auto-approve flag and report
                // `ApproveAllForSession` so the audit log records the
                // session-wide grant — mirroring the CLI sink, which returns
                // `ApproveAllForSession` after flipping its flag. A plain
                // "Grant for this session" only approves this single call and
                // leaves auto-approve off (per-call prompting).
                if decisions.iter().any(|d| matches!(d, GrantDecision::GrantedPersistent)) {
                    self.auto_approve.store(true, Ordering::Relaxed);
                    ApprovalDecision::ApproveAllForSession
                } else {
                    ApprovalDecision::Approve
                }
            }
            None => ApprovalDecision::Deny,
        }
    }

    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {
        self.auto_approve.store(true, Ordering::Relaxed);
    }

    async fn request_ack(
        &self,
        session_id: Ulid,
        message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        // Fast path: auto-approve if enabled (mirrors the CLI sink).
        if self.auto_approve.load(Ordering::Relaxed) {
            return true;
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        // Queue the ack for display. The queue is depth-bounded (ADR-68 §6,
        // DEFERRED row 4): the active dialog plus at most one queued. A full
        // queue or unreadable state is an EXPLICIT refusal — the request is
        // never silently dropped, so the task aborts (fail-closed) rather than
        // proceeding without the acknowledgement it asked for.
        if let Err(error) = crate::widgets::capability_dialog::enqueue_ack(
            &self.pending_ack,
            crate::widgets::capability_dialog::PendingAck {
                session_id,
                message: message.to_string(),
                sender: tx,
            },
        ) {
            tracing::warn!(
                ?session_id,
                %error,
                "request_ack refused; aborting the task (fail-closed, ADR-68 H-04)"
            );
            // Surface the named refusal to the UI (rendered as an error toast)
            // so the user can tell "refused because the queue was full" from a
            // user cancel.
            let _ = self.bus.publish_raw(concerto_core::event::EventKind::ErrorOccurred {
                message: format!("Acknowledgement refused: {error}"),
            });
            return false;
        }

        // Surface the pending ack to the UI via the event bus so Iced redraws.
        // Global event: the ack queue is a single shared FIFO, so any window
        // shows the front entry; the pending entry carries `session_id` so
        // resolution is routed by session membership (`resolve_ack`).
        let _ = self.bus.publish_raw(concerto_core::event::EventKind::ApprovalRequested {
            tool_name: "ack".to_string(),
            timeout_secs: 0,
        });

        rx.await.unwrap_or(false)
    }

    async fn request_intent_confirmation(
        &self,
        question: String,
        options: &[RequestedOutcome],
        _cancel: CancellationToken,
    ) -> Option<RequestedOutcome> {
        // Nothing to confirm — mirror the trait default's conservative
        // read-only reading (the orchestrator treats None as read-only).
        if options.is_empty() {
            return None;
        }

        let (tx, rx) = tokio::sync::oneshot::channel::<Option<RequestedOutcome>>();

        {
            let mut guard = self.pending_intent.lock().unwrap_or_else(|e| e.into_inner());
            guard.push_back(crate::widgets::capability_dialog::PendingIntent {
                question,
                options: options.to_vec(),
                sender: tx,
            });
        }

        // Surface the pending request to the UI via the event bus so Iced
        // redraws and shows the dialog while the agent blocks on it. Global
        // event: intentionally unscoped (the confirmation carries no session
        // id), mirroring `request_ack`.
        let _ = self.bus.publish_raw(concerto_core::event::EventKind::ApprovalRequested {
            tool_name: "intent".to_string(),
            timeout_secs: 0,
        });

        // A dropped/never-answered dialog cancels the wait channel; fall back
        // to the conservative read-only `None`.
        rx.await.ok().flatten()
    }

    async fn request_plan_approval(
        &self,
        session_id: Ulid,
        plan_id: &str,
        question: String,
        plan_text: &str,
        created_at: time::OffsetDateTime,
        _cancel: CancellationToken,
    ) -> Option<PlanDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<PlanDecision>>();

        {
            let mut guard = self.pending_plan.lock().unwrap_or_else(|e| e.into_inner());
            guard.push_back(crate::widgets::capability_dialog::PendingPlan {
                session_id,
                plan_id: plan_id.to_string(),
                question,
                plan_text: plan_text.to_string(),
                created_at,
                sender: tx,
            });
        }

        // Surface the pending request to the UI via the event bus so Iced
        // redraws and shows the dialog while the agent blocks on it. Global
        // event, mirroring `request_ack` and the intent dialog, so any window
        // refreshes; the decision itself is matched back to the requesting run
        // by `(session_id, plan_id)` in `resolve_plan`.
        let _ = self.bus.publish_raw(concerto_core::event::EventKind::ApprovalRequested {
            tool_name: "intent:plan".to_string(),
            timeout_secs: 0,
        });

        // A dropped/never-answered dialog cancels the wait channel; fall back
        // to the conservative read-only `None`.
        rx.await.ok().flatten()
    }
}

pub(super) fn desktop_approval_sink(
    cap_pending: crate::widgets::capability_dialog::SharedPending,
    pending_ack: crate::widgets::capability_dialog::SharedPendingAck,
    pending_intent: crate::widgets::capability_dialog::SharedPendingIntent,
    pending_plan: crate::widgets::capability_dialog::SharedPendingPlan,
    bus: EventBus,
) -> Arc<dyn ApprovalSink> {
    Arc::new(DesktopApprovalSink {
        cap_pending,
        pending_ack,
        pending_intent,
        pending_plan,
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus,
    })
}

/// Build the assistant chat message shown when an agent run completes.
///
/// Always lists the concrete files actually changed (from real tool results),
/// never the model's own completion claims. This is the desktop half of the
/// guarantee that text-only provider responses are never presented as
/// successful assistant output for action-required work.
///
/// The final answer is composed by `AgentOutput::summary` from structured
/// execution data (files written, verification results, project root) plus
/// any optional model-authored notes — never from unverified provider prose.
pub(crate) fn format_run_summary(output: &AgentOutput) -> String {
    output.summary()
}
