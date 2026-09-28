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

/// Shared channel for delivering the user's decision. A `watch` (not a
/// `oneshot`) so an identical request can coalesce onto the same decision slot
/// (cloneable receiver) and a late decision still lands even when the original
/// requester dropped its awaiting future on a timeout.
type DecisionSender = tokio::sync::watch::Sender<Option<Vec<GrantDecision>>>;

/// Type of the coalescable decision receiver handed to each waiter.
pub type DecisionReceiver = tokio::sync::watch::Receiver<Option<Vec<GrantDecision>>>;

/// A pending capability approval request.
#[derive(Debug)]
pub struct PendingApproval {
    pub plugin: PluginManifest,
    pub capabilities: Vec<CapabilityRequest>,
    /// Coalescing key (tool/action identity + input). An identical request
    /// reuses this entry's receiver instead of stacking a duplicate dialog.
    pub key: String,
    pub sender: DecisionSender,
    /// Kept alongside the sender so the channel stays open (and the value is
    /// retained) even when no waiter is attached.
    pub receiver: DecisionReceiver,
}

/// Shared pending-approval queue — a FIFO queue so concurrent multi-agent
/// requests do not overwrite each other (each distinct action gets its own
/// decision slot; identical actions coalesce onto one).
pub type SharedPending = Arc<Mutex<VecDeque<PendingApproval>>>;

/// Await a capability decision on a coalescable receiver. Returns the decision
/// immediately when it was already delivered, otherwise waits for the change.
/// `None` when the dialog was dropped without a decision.
pub async fn await_decision(mut receiver: DecisionReceiver) -> Option<Vec<GrantDecision>> {
    loop {
        if let Some(decisions) = receiver.borrow().clone() {
            return Some(decisions);
        }
        if receiver.changed().await.is_err() {
            return None;
        }
    }
}

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
    /// Session the ack belongs to, so resolution can confirm membership
    /// without consulting the executor (ADR-68, audit H-04). A cross-session
    /// or stale resolution can never answer a different run's ack.
    pub session_id: Ulid,
    pub message: String,
    pub sender: AckSender,
}

/// Maximum number of acknowledgement requests the desktop will hold pending at
/// once — the active dialog plus at most one queued behind it.
///
/// ADR-68 §6 settled this policy: the UI "queues at most one pending ack beyond
/// the active one; overflow rejects with an explicit error." The bound is
/// deliberately tiny because an ack is a *blocking, user-facing* confirmation,
/// not a background job:
///
/// * One active dialog is what the user can actually read; a second queued acks
///   keeps a concurrent prompt (another run/session) from overwriting the first.
/// * A deeper queue would only delay the *second* prompt past the point where
///   the requester is still waiting on it, and would let an ack storm grow UI
///   state without bound. Two is the smallest depth that covers the real
///   concurrent case without becoming an ack buffer.
///
/// Overflow is an explicit [`AckQueueError::QueueFull`] rejection, never a
/// silent drop.
pub const MAX_PENDING_ACKS: usize = 2;

/// Why an acknowledgement request could not be queued for display.
///
/// Both variants are fail-closed: the caller must abort the task rather than
/// proceed without the acknowledgement it asked for. Neither is a silent drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckQueueError {
    /// The bounded acknowledgement queue already holds [`MAX_PENDING_ACKS`]
    /// entries. The new request is refused outright — it never displaces an
    /// already-pending ack and is never dropped without a signal.
    QueueFull { capacity: usize },
    /// The shared queue state could not be read (its lock is poisoned), so the
    /// desktop cannot prove the request was recorded. Failing closed beats
    /// reporting "no pending acks" for a request that was actually made.
    StateUnavailable,
}

impl std::fmt::Display for AckQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull { capacity } => write!(
                f,
                "acknowledgement queue is full ({capacity} pending); refusing the new ack"
            ),
            Self::StateUnavailable => {
                write!(f, "acknowledgement queue state is unavailable; refusing the new ack")
            }
        }
    }
}

impl std::error::Error for AckQueueError {}

/// Shared pending-ack queue — a depth-bounded FIFO so a second acknowledgement
/// arriving while the first is displayed is queued rather than overwriting it
/// (ADR-68 §6, DEFERRED row 4). The front entry is the active dialog; entries
/// carry their owning session id and resolution is session-gated, so a stale or
/// cross-session entry can never answer another run's prompt.
pub type SharedPendingAck = Arc<Mutex<VecDeque<PendingAck>>>;

/// Create a new shared pending-ack queue.
pub fn shared_pending_ack() -> SharedPendingAck {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Queue an acknowledgement for display, honouring [`MAX_PENDING_ACKS`].
///
/// Returns [`AckQueueError::QueueFull`] when the bound is already reached: the
/// new request is rejected — never dropped silently and never by displacing an
/// existing ack. Returns [`AckQueueError::StateUnavailable`] when the shared
/// state cannot be read, so the caller can fail closed rather than assume the
/// request was shown.
pub fn enqueue_ack(state: &SharedPendingAck, ack: PendingAck) -> Result<(), AckQueueError> {
    // A poisoned lock means the queue is unreadable; refuse rather than guess.
    let mut guard = match state.lock() {
        Ok(guard) => guard,
        Err(_) => return Err(AckQueueError::StateUnavailable),
    };
    if guard.len() >= MAX_PENDING_ACKS {
        return Err(AckQueueError::QueueFull { capacity: MAX_PENDING_ACKS });
    }
    guard.push_back(ack);
    Ok(())
}

/// User action on the acknowledgement dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDialogMessage {
    /// User confirms they understand the risk and wants to continue.
    Acknowledge,
    /// User cancels the operation.
    Cancel,
}

/// Position indicator for the acknowledgement dialog.
///
/// `None` while a single ack is pending, so the common case renders exactly as
/// before. `Some("Acknowledgement 1 of N pending")` when more than one ack is
/// queued, so the user can see they have N outstanding acknowledgements rather
/// than believing the displayed one is the only prompt. The front entry is
/// always position 1 (FIFO); resolution drains the queue in order.
fn ack_queue_position(pending: usize) -> Option<String> {
    if pending <= 1 {
        return None;
    }
    Some(format!("Acknowledgement 1 of {pending} pending"))
}

/// Render the acknowledgement dialog.
///
/// Returns `None` when there is no pending request.
pub fn ack_view(state: &SharedPendingAck) -> Option<Element<'static, AckDialogMessage>> {
    let (message, pending) = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        let ack = guard.front()?;
        (ack.message.clone(), guard.len())
    };

    let header = text("⚠  Warning").size(20);

    let body = text(message).size(14);

    let continue_btn = button(text("Continue anyway")).on_press(AckDialogMessage::Acknowledge);

    let cancel_btn = button(text("Cancel operation")).on_press(AckDialogMessage::Cancel);

    let buttons = row![cancel_btn, continue_btn].spacing(10).padding(10);

    // The count/position line is added only when more than one ack is pending:
    // a single ack keeps the original modal layout unchanged (DEFERRED row 4).
    let mut children: Vec<Element<'static, AckDialogMessage>> = vec![header.into(), body.into()];
    if let Some(position) = ack_queue_position(pending) {
        children.push(text(position).size(12).into());
    }
    children.push(buttons.into());

    let content = column(children).spacing(12).padding(24).width(460);

    let surface = container(content)
        .width(500)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(crate::ui::container::modal);

    Some(surface.into())
}

/// Apply a user decision to the pending ack state.
///
/// Resolves the front (active) pending ack — but only when it belongs to
/// `session_id`: a stale or cross-session entry must never answer a different
/// run's prompt (ADR-68, audit H-04). A non-matching entry is restored to the
/// front so the owning session still sees its dialog. Returns `true` when the
/// matching entry was resolved.
pub fn resolve_ack(state: &SharedPendingAck, session_id: Ulid, acknowledged: bool) -> bool {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = guard.pop_front() else {
        return false;
    };
    if pending.session_id != session_id {
        // A different (stale/cross-session) entry reached the front — never
        // answer it; restore it so the owning session still sees its dialog.
        guard.push_front(pending);
        return false;
    }
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

    let _ = pending.sender.send(Some(decisions));
    true
}

// ---------------------------------------------------------------------------
// Intent confirmation dialog (ADR-55 §2)
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
// Plan approval dialog (ADR-55 §4)
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

    #[test]
    fn ack_queue_position_is_none_for_a_single_pending_ack() {
        assert_eq!(ack_queue_position(0), None);
        assert_eq!(ack_queue_position(1), None, "the common single-ack case renders unchanged");
    }

    #[test]
    fn ack_queue_position_counts_pending_acks() {
        assert_eq!(ack_queue_position(2), Some("Acknowledgement 1 of 2 pending".to_string()));
        assert_eq!(ack_queue_position(3), Some("Acknowledgement 1 of 3 pending".to_string()));
    }

    #[test]
    fn resolve_ack_matching_session_delivers_decision() {
        let state = shared_pending_ack();
        let session_id = Ulid::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        enqueue_ack(&state, PendingAck { session_id, message: "warning".into(), sender: tx })
            .expect("queue has room");

        assert!(resolve_ack(&state, session_id, true), "matching session must resolve");
        assert_eq!(rx.blocking_recv(), Ok(true));
        assert!(
            state.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "resolved ack must leave the queue empty"
        );
    }

    #[test]
    fn resolve_ack_wrong_session_leaves_pending() {
        let state = shared_pending_ack();
        let owning_session = Ulid::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        enqueue_ack(
            &state,
            PendingAck { session_id: owning_session, message: "warning".into(), sender: tx },
        )
        .expect("queue has room");

        // A different session's resolution must never answer this ack
        // (ADR-68, audit H-04).
        assert!(!resolve_ack(&state, Ulid::new(), false), "cross-session resolve must be rejected");
        assert!(
            !state.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the owning session's ack must stay pending"
        );

        // The owner can still resolve it.
        assert!(resolve_ack(&state, owning_session, false), "the owner resolve succeeds");
        assert_eq!(rx.blocking_recv(), Ok(false));
    }

    /// The queue is bounded: filling it to [`MAX_PENDING_ACKS`] succeeds, and
    /// the next request is refused with a named error instead of being dropped
    /// or displacing an existing entry.
    #[test]
    fn ack_queue_depth_bound_refuses_n_plus_one_with_named_error() {
        let state = shared_pending_ack();
        let mut receivers = Vec::new();
        for _ in 0..MAX_PENDING_ACKS {
            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            receivers.push(rx);
            enqueue_ack(
                &state,
                PendingAck { session_id: Ulid::new(), message: "warning".into(), sender: tx },
            )
            .expect("the bounded queue has room up to its capacity");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        let refused = enqueue_ack(
            &state,
            PendingAck { session_id: Ulid::new(), message: "overflow warning".into(), sender: tx },
        );
        assert_eq!(
            refused,
            Err(AckQueueError::QueueFull { capacity: MAX_PENDING_ACKS }),
            "the N+1th request must be refused with the named error"
        );
        assert_eq!(
            state.lock().unwrap_or_else(|e| e.into_inner()).len(),
            MAX_PENDING_ACKS,
            "a refused request must neither grow the queue nor displace an entry"
        );
    }

    /// Acks resolve FIFO: the entry raised first is delivered first.
    #[test]
    fn ack_queue_resolves_in_fifo_order() {
        let state = shared_pending_ack();
        let first_session = Ulid::new();
        let second_session = Ulid::new();
        let (tx_first, rx_first) = tokio::sync::oneshot::channel::<bool>();
        let (tx_second, rx_second) = tokio::sync::oneshot::channel::<bool>();
        enqueue_ack(
            &state,
            PendingAck { session_id: first_session, message: "first".into(), sender: tx_first },
        )
        .expect("queue has room");
        enqueue_ack(
            &state,
            PendingAck { session_id: second_session, message: "second".into(), sender: tx_second },
        )
        .expect("queue has room");

        // The second session cannot jump the queue: the front (first) is the
        // only resoluble entry until it is answered.
        assert!(
            !resolve_ack(&state, second_session, true),
            "the queued second ack must not resolve before the active first ack"
        );
        assert!(resolve_ack(&state, first_session, true), "the first ack resolves first");
        assert_eq!(rx_first.blocking_recv(), Ok(true));
        assert!(resolve_ack(&state, second_session, false), "the second ack resolves next");
        assert_eq!(rx_second.blocking_recv(), Ok(false));
    }

    /// Fail-closed: an unreadable queue state refuses the request with a named
    /// error rather than reporting that nothing is pending.
    #[test]
    fn enqueue_ack_refuses_when_state_is_unavailable() {
        let state = shared_pending_ack();
        let poisoned = state.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("queue lock");
            panic!("poison the ack queue lock");
        })
        .join();

        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        let refused = enqueue_ack(
            &state,
            PendingAck { session_id: Ulid::new(), message: "warning".into(), sender: tx },
        );
        assert_eq!(
            refused,
            Err(AckQueueError::StateUnavailable),
            "an unreadable queue must refuse, never silently accept"
        );
    }

    /// Fail-closed waiting primitive: a dropped decision sender (the dialog
    /// went away with the run) resolves the wait as `None` — which the sink
    /// maps to a denial — instead of hanging forever.
    #[tokio::test]
    async fn await_decision_on_a_closed_channel_returns_none() {
        let (sender, receiver) = tokio::sync::watch::channel::<Option<Vec<GrantDecision>>>(None);
        drop(sender);

        let decision =
            tokio::time::timeout(std::time::Duration::from_secs(5), await_decision(receiver))
                .await
                .expect("a closed decision channel must resolve, never hang");
        assert!(decision.is_none(), "an unanswerable capability dialog must deny");
    }
}
