//! Capability approval dialog — shown when a WASM plugin requests
//! capabilities that have not yet been granted.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length};

use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome};
use concerto_plugins::capability::GrantDecision;
use time::OffsetDateTime;

/// Shared channel for delivering the user's decision.
type DecisionSender = tokio::sync::oneshot::Sender<Vec<GrantDecision>>;

/// A pending capability approval request.
#[derive(Debug)]
pub struct PendingApproval {
    pub plugin: PluginManifest,
    pub capabilities: Vec<CapabilityRequest>,
    pub sender: DecisionSender,
}

/// Shared pending-approval queue — a FIFO queue so concurrent multi-agent
/// requests do not overwrite each other (each gets its own oneshot channel).
pub type SharedPending = Arc<Mutex<VecDeque<PendingApproval>>>;

/// Create a new shared pending-approval queue.
pub fn shared_pending() -> SharedPending {
    Arc::new(Mutex::new(VecDeque::new()))
}

// ---------------------------------------------------------------------------
// Acknowledgement dialog (non-undo warning)
// ---------------------------------------------------------------------------

/// Shared channel for delivering the user's acknowledgement decision.
type AckSender = tokio::sync::oneshot::Sender<bool>;

/// A pending acknowledgement request — shown when git undo is unavailable.
#[derive(Debug)]
pub struct PendingAck {
    pub message: String,
    pub sender: AckSender,
}

/// Shared pending-ack state (None = no dialog shown).
pub type SharedPendingAck = Arc<Mutex<Option<PendingAck>>>;

/// Create a new shared pending-ack cell.
pub fn shared_pending_ack() -> SharedPendingAck {
    Arc::new(Mutex::new(None))
}

/// User action on the acknowledgement dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDialogMessage {
    /// User confirms they understand the risk and wants to continue.
    Acknowledge,
    /// User cancels the operation.
    Cancel,
}

/// Render the acknowledgement dialog.
///
/// Returns `None` when there is no pending request.
pub fn ack_view(state: &SharedPendingAck) -> Option<Element<'static, AckDialogMessage>> {
    let message = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        let ack = guard.as_ref()?;
        ack.message.clone()
    };

    let header = text("⚠  Warning").size(20);

    let body = text(message).size(14);

    let continue_btn = button(text("Continue anyway")).on_press(AckDialogMessage::Acknowledge);

    let cancel_btn = button(text("Cancel operation")).on_press(AckDialogMessage::Cancel);

    let buttons = row![cancel_btn, continue_btn].spacing(10).padding(10);

    let content = column![header, body, buttons].spacing(12).padding(24).width(460);

    let surface = container(content)
        .width(500)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(crate::ui::container::modal);

    Some(surface.into())
}

/// Apply a user decision to the pending ack state.
pub fn resolve_ack(state: &SharedPendingAck, acknowledged: bool) -> bool {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = guard.take() else {
        return false;
    };
    let _ = pending.sender.send(acknowledged);
    true
}

// ---------------------------------------------------------------------------
// Dialog message
// ---------------------------------------------------------------------------

/// User action on the capability dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// Grant for this session only.
    GrantSession,
    /// Grant persistently (always allow).
    GrantAlways,
    /// Deny this request.
    Deny,
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// Render the capability approval dialog.
///
/// Returns `None` when there is no pending request — the caller should skip
/// rendering the dialog overlay entirely.
pub fn view(state: &SharedPending) -> Option<Element<'static, Message>> {
    // Extract only the data we need while holding the lock, so we can
    // drop the guard before building the widget tree (avoids lifetime
    // issues with the MutexGuard).
    let (plugin_name, plugin_desc, capabilities) = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        let approval = guard.front()?;
        (
            approval.plugin.name.clone(),
            approval.plugin.description.clone(),
            approval.capabilities.clone(),
        )
    };

    let header = text(format!("\u{201c}{}\u{201d} requests permissions", plugin_name)).size(20);

    let desc = text(plugin_desc).size(14);

    let mut cap_items: Vec<Element<'static, Message>> = Vec::new();
    for cap in &capabilities {
        let label = match cap {
            CapabilityRequest::FilesystemRead { .. } => "\u{1f4d6} Read files".to_string(),
            CapabilityRequest::FilesystemWrite { .. } => {
                "\u{270f}\u{fe0f}  Write files".to_string()
            }
            CapabilityRequest::NetworkOutbound { .. } => "\u{1f310} Network access".to_string(),
            CapabilityRequest::ShellExecute { .. } => "\u{26a1} Execute commands".to_string(),
            CapabilityRequest::Other { description } => description.clone(),
            _ => "Unknown capability".to_string(),
        };
        cap_items.push(text(label).size(14).into());
    }

    let cap_list = column(cap_items).spacing(4).padding(8);

    let details = column![text("Capabilities requested:").size(16), cap_list,].spacing(8);

    let grant_btn = button(text("Grant for this session"))
        .style(crate::ui::button::primary)
        .on_press(Message::GrantSession);

    let persist_btn = button(text("Always allow"))
        .style(crate::ui::button::primary)
        .on_press(Message::GrantAlways);

    let deny_btn = button(text("Deny")).style(crate::ui::button::danger).on_press(Message::Deny);

    let buttons = row![deny_btn, grant_btn, persist_btn].spacing(10).padding(10);

    let content = column![header, desc, details, buttons].spacing(12).padding(24).width(460);

    let surface = container(scrollable(content))
        .width(500)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(crate::ui::container::modal);

    Some(surface.into())
}

/// Apply a user decision to the pending approval state.
///
/// Pops the pending request, builds the decision list, and sends it through
/// the oneshot channel. Returns `true` if a request was pending and was
/// resolved.
pub fn resolve(state: &SharedPending, decision: &Message) -> bool {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = guard.pop_front() else {
        return false;
    };

    let decisions: Vec<GrantDecision> = pending
        .capabilities
        .iter()
        .map(|_| match decision {
            Message::GrantSession => GrantDecision::Granted,
            Message::GrantAlways => GrantDecision::GrantedPersistent,
            Message::Deny => GrantDecision::Denied,
        })
        .collect();

    let _ = pending.sender.send(decisions);
    true
}

// ---------------------------------------------------------------------------
// Intent confirmation dialog (ADR-55 §1)
// ---------------------------------------------------------------------------
//
// The run loop asks the user to confirm a change of run intent before letting
// a mutation proceed. Mirrors the capability/ack dialog mechanism: the sink
// queues a [`PendingIntent`] and awaits its oneshot channel, the app renders
// [`intent_view`] as a modal, and [`resolve_intent`] delivers the picked
// outcome (or `None` for cancel).

/// Shared channel for delivering the user's chosen outcome.
type IntentSender = tokio::sync::oneshot::Sender<Option<RequestedOutcome>>;

/// A pending intent confirmation request.
#[derive(Debug)]
pub struct PendingIntent {
    pub question: String,
    pub options: Vec<RequestedOutcome>,
    pub sender: IntentSender,
}

/// Shared pending-intent queue — a FIFO queue mirroring [`SharedPending`] so
/// concurrent requests (e.g. a multi-agent batch) do not overwrite each other;
/// each gets its own oneshot channel.
pub type SharedPendingIntent = Arc<Mutex<VecDeque<PendingIntent>>>;

/// Create a new shared pending-intent queue.
pub fn shared_pending_intent() -> SharedPendingIntent {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// User action on the intent confirmation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentDialogMessage {
    /// User picked one of the offered outcome options.
    Select(RequestedOutcome),
    /// User rejected the confirmation (returns `None` → read-only run).
    Cancel,
}

/// Human-readable button label for a requested outcome. `Answer` renders as
/// "Chat" so the dialog always shows an obvious conversational option; the
/// rest of the Phase-0 set maps to its enum name. Unknown future variants
/// fall back to their `Debug` name so the dialog stays non-exhaustive-safe.
fn outcome_label(outcome: RequestedOutcome) -> String {
    match outcome {
        RequestedOutcome::Answer => "Chat".to_string(),
        RequestedOutcome::Diagnose => "Diagnose".to_string(),
        RequestedOutcome::Review => "Review".to_string(),
        RequestedOutcome::Plan => "Plan".to_string(),
        RequestedOutcome::Execute => "Execute".to_string(),
        RequestedOutcome::Verify => "Verify".to_string(),
        _ => format!("{outcome:?}"),
    }
}

/// Render the intent confirmation dialog.
///
/// Returns `None` when there is no pending request — the caller should skip
/// rendering the dialog overlay entirely.
pub fn intent_view(state: &SharedPendingIntent) -> Option<Element<'static, IntentDialogMessage>> {
    // Extract only the data we need while holding the lock, then drop the
    // guard before building the widget tree.
    let (question, options) = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        let intent = guard.front()?;
        (intent.question.clone(), intent.options.clone())
    };

    let header = text("Confirm intent").size(20);

    let body = text(question).size(14);

    let option_buttons: Vec<Element<'static, IntentDialogMessage>> = options
        .iter()
        .map(|outcome| {
            button(text(outcome_label(*outcome)))
                .style(crate::ui::button::primary)
                .on_press(IntentDialogMessage::Select(*outcome))
                .into()
        })
        .collect();

    let cancel_btn = button(text("Cancel"))
        .style(crate::ui::button::secondary)
        .on_press(IntentDialogMessage::Cancel);

    let content = column![header, body, column(option_buttons).spacing(8), cancel_btn,]
        .spacing(12)
        .padding(24)
        .width(460);

    let surface = container(scrollable(content))
        .width(500)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(crate::ui::container::modal);

    Some(surface.into())
}

/// Apply a user decision to the pending intent state.
///
/// Pops the pending request and sends the selected outcome (or `None` on
/// cancel) through the oneshot channel. Returns `true` if a request was
/// pending and was resolved.
pub fn resolve_intent(state: &SharedPendingIntent, message: IntentDialogMessage) -> bool {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = guard.pop_front() else {
        return false;
    };

    let selected = match message {
        IntentDialogMessage::Select(outcome) => Some(outcome),
        IntentDialogMessage::Cancel => None,
    };

    let _ = pending.sender.send(selected);
    true
}

// ---------------------------------------------------------------------------
// Plan approval dialog (ADR-55 Phase 1d)
// ---------------------------------------------------------------------------
//
// Mirrors the intent dialog: the sink queues a [`PendingPlan`] and awaits its
// oneshot channel, the app renders [`plan_view`] as a modal, and
// [`resolve_plan`] delivers the user's decision — but only for the
// `(session_id, plan_id)` the dialog was shown for, so a stale or cross-session
// queue entry can never answer a different prompt.

/// Shared channel for delivering the user's plan decision.
type PlanSender = tokio::sync::oneshot::Sender<Option<PlanDecision>>;

/// A pending plan-approval request.
#[derive(Debug)]
pub struct PendingPlan {
    /// Session the run belongs to, so a prompt can never be answered by a
    /// different session's run.
    pub session_id: Ulid,
    /// Binding identifier reported in the audit row.
    pub plan_id: String,
    /// The question asked (a stored plan exists — apply or replan?).
    pub question: String,
    /// The stored plan body (capped at 16 KiB upstream), rendered in a
    /// scrollable region so the decision is made against the actual plan.
    pub plan_text: String,
    /// When the plan was recorded, UTC — surfaced as a relative age in the
    /// dialog header so the decision is made against a plan the user can
    /// situate in time.
    pub created_at: OffsetDateTime,
    pub sender: PlanSender,
}

/// Shared pending-plan queue — a FIFO queue mirroring [`SharedPendingIntent`]
/// so concurrent requests cannot overwrite each other; each gets its own
/// oneshot channel.
pub type SharedPendingPlan = Arc<Mutex<VecDeque<PendingPlan>>>;

/// Create a new shared pending-plan queue.
pub fn shared_pending_plan() -> SharedPendingPlan {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// User action on the plan approval dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDialogMessage {
    /// Apply the previously approved plan now (mutation-capable).
    Apply,
    /// Discard the stored plan and plan this objective anew (read-only).
    Replan,
    /// Dismiss the dialog (read-only; no decision).
    Cancel,
}

/// Compact display label for a plan id: the id is a ULID that would overflow
/// the dialog header, so only its leading run is shown.
fn plan_id_label(plan_id: &str) -> String {
    const MAX_LEN: usize = 12;
    if plan_id.chars().count() <= MAX_LEN {
        return plan_id.to_owned();
    }
    let truncated: String = plan_id.chars().take(MAX_LEN).collect();
    format!("{truncated}…")
}

/// Human-friendly age of a plan for the approval dialog header: "made just
/// now" under a minute, then "Nm ago", "Nh ago", and "Nd ago" past the first
/// day. Computed from the UTC recording time; elapsed seconds are floored at
/// zero so a clock-skewed future timestamp can never read as "negative age".
fn relative_age(created_at: OffsetDateTime) -> String {
    let elapsed = OffsetDateTime::now_utc() - created_at;
    let seconds = elapsed.whole_seconds().max(0);
    if seconds < 60 {
        return "made just now".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

/// Render the plan approval dialog.
///
/// Returns `None` when there is no pending request — the caller should skip
/// rendering the dialog overlay entirely.
pub fn plan_view(
    state: &SharedPendingPlan,
    theme: &crate::theme::AppTheme,
) -> Option<Element<'static, PlanDialogMessage>> {
    // Extract only the data we need while holding the lock, then drop the
    // guard before building the widget tree.
    let (question, plan_id, plan_text, created_at) = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        let plan = guard.front()?;
        (plan.question.clone(), plan.plan_id.clone(), plan.plan_text.clone(), plan.created_at)
    };
    let palette = &theme.palette;
    let ts = &theme.type_scale;

    // The header shows how long ago the plan was recorded, so Apply is a
    // decision about a plan the user can situate in time.
    let header =
        text(format!("Plan · {}", relative_age(created_at))).size(ts.title).color(palette.text);

    let body = text(question).size(ts.body);

    let plan_label = text(format!("Plan ({})", plan_id_label(&plan_id))).size(ts.label);

    // The full plan id as a low-emphasis secondary line — the header label is
    // truncated to fit, but the audit identity stays visible in full.
    let full_plan_id = text(plan_id).size(ts.caption).color(palette.text_muted);

    // The stored plan body can be up to 16 KiB, so it renders inside a
    // bounded-height scrollable region — never a collapsed tooltip — so the
    // decision is made against the actual plan text.
    let plan_body =
        scrollable(text(plan_text).size(ts.body)).width(Length::Fill).height(Length::Fixed(220.0));

    let apply_btn =
        button(text("Apply")).style(crate::ui::button::primary).on_press(PlanDialogMessage::Apply);

    let replan_btn = button(text("Re-plan"))
        .style(crate::ui::button::secondary)
        .on_press(PlanDialogMessage::Replan);

    let cancel_btn = button(text("Cancel"))
        .style(crate::ui::button::secondary)
        .on_press(PlanDialogMessage::Cancel);

    let buttons = row![apply_btn, replan_btn, cancel_btn].spacing(10).padding(10);

    let content = column![header, body, plan_label, full_plan_id, plan_body, buttons]
        .spacing(12)
        .padding(24)
        .width(520);

    let surface = container(content)
        .width(560)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(crate::ui::container::modal);

    Some(surface.into())
}

/// Apply a user decision to the pending plan state.
///
/// Pops the front pending request and sends the decision (or `None` on
/// dismiss) through the oneshot channel — but only when that request matches
/// `(session_id, plan_id)`: a stale or cross-session queue entry must never
/// answer a different prompt. A non-matching entry is left queued so the
/// owning session's dialog stays visible. Returns `true` when the matching
/// entry was resolved.
pub fn resolve_plan(
    state: &SharedPendingPlan,
    session_id: Ulid,
    plan_id: &str,
    message: PlanDialogMessage,
) -> bool {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = guard.pop_front() else {
        return false;
    };
    if pending.session_id != session_id || pending.plan_id != plan_id {
        // A different (stale/cross-session) entry reached the front — never
        // answer it; restore it so the owning session still sees its dialog.
        guard.push_front(pending);
        return false;
    }

    let decision = match message {
        PlanDialogMessage::Apply => Some(PlanDecision::Apply),
        PlanDialogMessage::Replan => Some(PlanDecision::Replan),
        PlanDialogMessage::Cancel => None,
    };

    let _ = pending.sender.send(decision);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_age_under_a_minute_is_just_now() {
        let created = OffsetDateTime::now_utc() - time::Duration::seconds(30);
        assert_eq!(relative_age(created), "made just now");
    }

    #[test]
    fn relative_age_minutes_ago() {
        let created = OffsetDateTime::now_utc() - time::Duration::minutes(5);
        assert_eq!(relative_age(created), "5m ago");
    }

    #[test]
    fn relative_age_hours_ago() {
        let created = OffsetDateTime::now_utc() - time::Duration::hours(3);
        assert_eq!(relative_age(created), "3h ago");
    }

    #[test]
    fn relative_age_days_ago() {
        let created = OffsetDateTime::now_utc() - time::Duration::days(2);
        assert_eq!(relative_age(created), "2d ago");
    }

    #[test]
    fn relative_age_future_timestamp_does_not_underflow() {
        let created = OffsetDateTime::now_utc() + time::Duration::seconds(120);
        assert_eq!(relative_age(created), "made just now");
    }
}
