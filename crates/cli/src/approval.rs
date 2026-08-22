use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome};
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::types::PolicyAction;
use concerto_core::CancellationToken;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub tool_name: String,
    pub detail: String,
    pub acknowledgement: bool,
}

struct PendingApproval {
    prompt: ApprovalPrompt,
    responder: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// The intent confirmation question plus its selectable outcomes, exposed to
/// the TUI so it can render the modal and translate keypresses back into a
/// decision (ADR-55 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentPrompt {
    pub question: String,
    pub options: Vec<RequestedOutcome>,
}

/// A pending intent-confirmation request, delivered through the same oneshot
/// pattern as [`PendingApproval`]: the sink awaits the receiver while the TUI
/// renders [`IntentPrompt`] and calls [`CliApprovalState::resolve_intent`].
struct PendingIntent {
    question: String,
    options: Vec<RequestedOutcome>,
    responder: tokio::sync::oneshot::Sender<Option<RequestedOutcome>>,
}

/// The plan-approval question plus its identity, exposed to the TUI so it can
/// render the modal and translate keypresses back into a decision (ADR-55
/// Phase 1d). `session_id` + `plan_id` gate resolution so a stale or
/// cross-session request can never be answered by the wrong run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrompt {
    pub session_id: Ulid,
    pub plan_id: String,
    pub question: String,
    pub plan_text: String,
}

/// A pending plan-approval request, delivered through the same oneshot pattern
/// as [`PendingIntent`]: the sink awaits the receiver while the TUI renders
/// [`PlanPrompt`] and calls [`CliApprovalState::resolve_plan`].
struct PendingPlan {
    session_id: Ulid,
    plan_id: String,
    question: String,
    plan_text: String,
    responder: tokio::sync::oneshot::Sender<Option<PlanDecision>>,
}

#[derive(Clone, Default)]
pub struct CliApprovalState {
    pending: Arc<Mutex<Option<PendingApproval>>>,
    pending_intent: Arc<Mutex<Option<PendingIntent>>>,
    pending_plan: Arc<Mutex<Option<PendingPlan>>>,
}

impl CliApprovalState {
    pub fn prompt(&self) -> Option<ApprovalPrompt> {
        self.pending
            .lock()
            .ok()
            .and_then(|pending| pending.as_ref().map(|request| request.prompt.clone()))
    }

    pub fn resolve(&self, decision: ApprovalDecision) {
        let pending = self.pending.lock().ok().and_then(|mut pending| pending.take());
        if let Some(pending) = pending {
            let _ = pending.responder.send(decision);
        }
    }

    fn request(
        &self,
        prompt: ApprovalPrompt,
    ) -> Option<tokio::sync::oneshot::Receiver<ApprovalDecision>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut pending = self.pending.lock().ok()?;
        if pending.is_some() {
            return None;
        }
        *pending = Some(PendingApproval { prompt, responder: sender });
        Some(receiver)
    }

    /// Snapshot of the pending intent confirmation, if any, for the TUI to
    /// render.
    pub fn intent_prompt(&self) -> Option<IntentPrompt> {
        self.pending_intent.lock().ok().and_then(|pending| {
            pending.as_ref().map(|request| IntentPrompt {
                question: request.question.clone(),
                options: request.options.clone(),
            })
        })
    }

    /// Resolve the pending intent confirmation with the user's selection
    /// (`Some(outcome)` to confirm, `None` to reject). A no-op when nothing is
    /// pending.
    pub fn resolve_intent(&self, selected: Option<RequestedOutcome>) {
        let pending = self.pending_intent.lock().ok().and_then(|mut pending| pending.take());
        if let Some(pending) = pending {
            let _ = pending.responder.send(selected);
        }
    }

    /// Install a pending intent confirmation. Returns `None` when another
    /// intent is already pending (a caller with no receiver falls back to the
    /// conservative read-only `None` outcome).
    fn request_intent(
        &self,
        question: String,
        options: &[RequestedOutcome],
    ) -> Option<tokio::sync::oneshot::Receiver<Option<RequestedOutcome>>> {
        // Nothing to confirm — mirror the trait default's conservative
        // read-only reading.
        if options.is_empty() {
            return None;
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut pending = self.pending_intent.lock().ok()?;
        if pending.is_some() {
            return None;
        }
        *pending = Some(PendingIntent { question, options: options.to_vec(), responder: sender });
        Some(receiver)
    }

    /// Snapshot of the pending plan approval, if any, for the TUI to render.
    pub fn plan_prompt(&self) -> Option<PlanPrompt> {
        self.pending_plan.lock().ok().and_then(|pending| {
            pending.as_ref().map(|request| PlanPrompt {
                session_id: request.session_id,
                plan_id: request.plan_id.clone(),
                question: request.question.clone(),
                plan_text: request.plan_text.clone(),
            })
        })
    }

    /// Resolve the pending plan approval with the user's decision, but ONLY
    /// when the pending request matches `(session_id, plan_id)`: a stale or
    /// cross-session request must never be answered by the wrong run.
    /// `Some(Apply)` / `Some(Replan)` confirm the corresponding choice; `None`
    /// dismisses (conservative read-only). Returns `true` when a matching
    /// request was resolved; a no-op (the pending request stays in place) when
    /// nothing is pending or the identity mismatches.
    pub fn resolve_plan(
        &self,
        session_id: Ulid,
        plan_id: &str,
        selected: Option<PlanDecision>,
    ) -> bool {
        let Ok(mut pending) = self.pending_plan.lock() else {
            return false;
        };
        if !pending
            .as_ref()
            .is_some_and(|request| request.session_id == session_id && request.plan_id == plan_id)
        {
            return false;
        }
        let Some(request) = pending.take() else {
            return false;
        };
        let _ = request.responder.send(selected);
        true
    }

    /// Install a pending plan approval. Returns `None` when another plan is
    /// already pending (a caller with no receiver falls back to the
    /// conservative read-only `None` outcome).
    fn request_plan(
        &self,
        session_id: Ulid,
        plan_id: &str,
        question: String,
        plan_text: String,
    ) -> Option<tokio::sync::oneshot::Receiver<Option<PlanDecision>>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut pending = self.pending_plan.lock().ok()?;
        if pending.is_some() {
            return None;
        }
        *pending = Some(PendingPlan {
            session_id,
            plan_id: plan_id.to_string(),
            question,
            plan_text,
            responder: sender,
        });
        Some(receiver)
    }
}

/// Interactive approval sink backed by a modal rendered inside the TUI.
pub struct CliApprovalSink {
    state: CliApprovalState,
    auto_approve: AtomicBool,
}

impl CliApprovalSink {
    pub fn new(auto_approve: bool) -> Self {
        Self { state: CliApprovalState::default(), auto_approve: AtomicBool::new(auto_approve) }
    }

    pub fn with_state(state: CliApprovalState) -> Self {
        Self { state, auto_approve: AtomicBool::new(false) }
    }

    /// Switch to auto-approve mode at runtime (e.g. when user selects "approve
    /// all for session").
    pub fn set_auto_approve(&self, val: bool) {
        self.auto_approve.store(val, Ordering::Relaxed);
    }
}

impl Default for CliApprovalSink {
    fn default() -> Self {
        Self::new(false)
    }
}

#[async_trait::async_trait]
impl ApprovalSink for CliApprovalSink {
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        // Fast path: auto-approve if enabled.
        if self.auto_approve.load(Ordering::Relaxed) {
            return ApprovalDecision::Approve;
        }

        let prompt = ApprovalPrompt {
            tool_name: action.tool_name.to_string(),
            detail: summarize_input(action),
            acknowledgement: false,
        };
        let decision = match self.state.request(prompt) {
            Some(receiver) => receiver.await.unwrap_or(ApprovalDecision::Deny),
            None => ApprovalDecision::Deny,
        };

        // If user chose "approve all for session", flip the auto-approve flag.
        if decision == ApprovalDecision::ApproveAllForSession {
            self.set_auto_approve(true);
        }

        decision
    }

    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {
        self.set_auto_approve(true);
    }

    async fn request_ack(&self, message: &str, _cancel: CancellationToken) -> bool {
        if self.auto_approve.load(Ordering::Relaxed) {
            return true;
        }
        let prompt = ApprovalPrompt {
            tool_name: "warning".to_string(),
            detail: message.to_string(),
            acknowledgement: true,
        };
        let decision = match self.state.request(prompt) {
            Some(receiver) => receiver.await.unwrap_or(ApprovalDecision::Deny),
            None => return false,
        };
        if decision == ApprovalDecision::ApproveAllForSession {
            self.set_auto_approve(true);
        }
        matches!(decision, ApprovalDecision::Approve | ApprovalDecision::ApproveAllForSession)
    }

    async fn request_intent_confirmation(
        &self,
        question: String,
        options: &[RequestedOutcome],
        _cancel: CancellationToken,
    ) -> Option<RequestedOutcome> {
        // Mirror the interactive approval prompt: queue the question and await
        // the TUI's keypress decision. `None` covers both a deliberate reject
        // (Esc/q) and a dropped/never-answered prompt, which the run loop
        // treats as read-only (ADR-55 §1).
        match self.state.request_intent(question, options) {
            Some(receiver) => receiver.await.unwrap_or(None),
            None => None,
        }
    }

    async fn request_plan_approval(
        &self,
        session_id: Ulid,
        plan_id: &str,
        question: String,
        plan_text: &str,
        _created_at: OffsetDateTime,
        _cancel: CancellationToken,
    ) -> Option<PlanDecision> {
        // Mirror the intent confirmation: queue the prompt and await the TUI's
        // keypress decision. `None` covers a drop (no receiver — the occupied
        // slot / conservative reading) and a dismissed prompt, both of which
        // the run loop treats as read-only (ADR-55 Phase 1d).
        match self.state.request_plan(session_id, plan_id, question, plan_text.to_string()) {
            Some(receiver) => receiver.await.unwrap_or(None),
            None => None,
        }
    }
}

/// Produce a short human-readable summary of the policy action's input.
fn summarize_input(action: &PolicyAction<'_>) -> String {
    if action.tool_name == "shell" {
        if let Some(command) = action.input.get("command").and_then(|value| value.as_str()) {
            let arguments = action
                .input
                .get("args")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>();
            if arguments.is_empty() {
                return format!("command: {command}");
            }
            return format!("command: {command} {}", arguments.join(" "));
        }
    }

    // Show the most relevant field depending on tool type.
    let key = match action.tool_name {
        "shell" => Some("command"),
        "filesystem" => Some("path"),
        "git" => Some("operation"),
        _ => None,
    };
    if let Some(k) = key {
        if let Some(val) = action.input.get(k).and_then(|v| v.as_str()) {
            return format!("{k}: {val}");
        }
    }
    // Fallback: truncated JSON.
    let json = serde_json::to_string(&action.input).unwrap_or_default();
    if json.len() > 120 {
        format!("{}…", &json[..119])
    } else {
        json
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::CapabilitySet;

    // ------------------------------------------------------------------
    // summarize_input
    // ------------------------------------------------------------------

    #[test]
    fn summarize_input_shell() {
        let input = serde_json::json!({"command": "rm", "args": ["-rf", "/tmp/foo"]});
        let action = PolicyAction {
            tool_name: "shell",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let summary = summarize_input(&action);
        assert_eq!(summary, "command: rm -rf /tmp/foo");
    }

    #[test]
    fn summarize_input_filesystem() {
        let input = serde_json::json!({"path": "/home/user/secret.txt"});
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let summary = summarize_input(&action);
        assert_eq!(summary, "path: /home/user/secret.txt");
    }

    #[test]
    fn summarize_input_unknown_tool() {
        let input = serde_json::json!({"foo": "bar"});
        let action = PolicyAction {
            tool_name: "mystery",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let summary = summarize_input(&action);
        assert!(summary.contains("foo"));
    }

    #[test]
    fn summarize_input_git() {
        let input = serde_json::json!({"operation": "commit", "message": "fix bug"});
        let action = PolicyAction {
            tool_name: "git",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let summary = summarize_input(&action);
        assert_eq!(summary, "operation: commit");
    }

    #[test]
    fn summarize_input_shell_no_args() {
        let input = serde_json::json!({"command": "ls"});
        let action = PolicyAction {
            tool_name: "shell",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let summary = summarize_input(&action);
        assert_eq!(summary, "command: ls");
    }

    #[test]
    fn summarize_input_long_json_is_truncated() {
        let input = serde_json::json!({"very_long_key": "x".repeat(200)});
        let action = PolicyAction {
            tool_name: "custom",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let summary = summarize_input(&action);
        // The truncated form takes the first 119 chars + Unicode ellipsis.
        assert!(summary.chars().count() <= 120, "summary should be truncated to ~120 chars");
        assert!(!summary.ends_with('}'), "truncated summary should not end with original content");
        assert!(summary.contains('…'), "truncated summary should contain ellipsis");
    }

    // ------------------------------------------------------------------
    // CliApprovalState
    // ------------------------------------------------------------------

    #[test]
    fn approval_state_request_resolve_round_trip() {
        let state = CliApprovalState::default();
        let prompt = ApprovalPrompt {
            tool_name: "shell".into(),
            detail: "command: ls".into(),
            acknowledgement: false,
        };

        let receiver = state.request(prompt.clone());
        assert!(receiver.is_some(), "first request should succeed");

        // Request while one is pending should return None.
        let second = state.request(ApprovalPrompt {
            tool_name: "git".into(),
            detail: "operation: commit".into(),
            acknowledgement: false,
        });
        assert!(second.is_none(), "second request while one is pending should return None");

        // Prompt inspection mirrors the pending request.
        let active = state.prompt();
        assert_eq!(active, Some(prompt));

        // Resolve with approve.
        state.resolve(ApprovalDecision::Approve);
        assert!(state.prompt().is_none(), "after resolve, no prompt should be pending");
    }

    #[test]
    fn approval_state_resolve_without_request_is_noop() {
        let state = CliApprovalState::default();
        assert!(state.prompt().is_none());
        // Resolve on empty state should not panic.
        state.resolve(ApprovalDecision::Deny);
        assert!(state.prompt().is_none());
    }

    // ------------------------------------------------------------------
    // CliApprovalSink (synchronous paths)
    // ------------------------------------------------------------------

    #[test]
    fn approval_sink_auto_approve_returns_approve() {
        let sink = CliApprovalSink::new(true);
        assert!(sink.auto_approve.load(Ordering::Relaxed));
    }

    #[test]
    fn approval_sink_default_is_not_auto_approve() {
        let sink = CliApprovalSink::default();
        assert!(!sink.auto_approve.load(Ordering::Relaxed));
    }

    #[test]
    fn approval_sink_set_auto_approve_toggles() {
        let sink = CliApprovalSink::new(false);
        assert!(!sink.auto_approve.load(Ordering::Relaxed));
        sink.set_auto_approve(true);
        assert!(sink.auto_approve.load(Ordering::Relaxed));
        sink.set_auto_approve(false);
        assert!(!sink.auto_approve.load(Ordering::Relaxed));
    }

    #[test]
    fn approval_sink_with_state_inherits_auto_approve() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state);
        assert!(!sink.auto_approve.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn approval_sink_auto_approve_skips_prompt() {
        let sink = CliApprovalSink::new(true);
        let action = PolicyAction {
            tool_name: "shell",
            input: &serde_json::json!({"command": "echo hi"}),
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let cancel = concerto_core::CancellationToken::new();
        let decision = sink.request_approval(&action, cancel.clone()).await;
        assert_eq!(decision, ApprovalDecision::Approve);
    }

    #[tokio::test]
    async fn approval_sink_auto_approve_ack_returns_true() {
        let sink = CliApprovalSink::new(true);
        let cancel = concerto_core::CancellationToken::new();
        let ack = sink.request_ack("some warning", cancel.clone()).await;
        assert!(ack);
    }

    #[tokio::test]
    async fn approval_sink_approve_all_sets_auto_approve() {
        let sink = CliApprovalSink::new(false);
        let cancel = concerto_core::CancellationToken::new();
        sink.approve_all_for_session(Ulid::new(), cancel.clone()).await;
        assert!(sink.auto_approve.load(Ordering::Relaxed));
    }

    // ------------------------------------------------------------------
    // request_intent_confirmation (ADR-55 §1)
    // ------------------------------------------------------------------

    /// Wait until the spawned sink future has installed the pending intent so
    /// the test resolves it without racing the spawn. Bounded so a broken sink
    /// fails the test instead of hanging forever.
    async fn wait_for_pending_intent(state: &CliApprovalState) {
        for _ in 0..500 {
            if state.intent_prompt().is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("intent confirmation never queued a prompt");
    }

    #[tokio::test]
    async fn intent_sink_round_trip_selects_outcome() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let cancel = concerto_core::CancellationToken::new();
        let options =
            vec![RequestedOutcome::Answer, RequestedOutcome::Diagnose, RequestedOutcome::Execute];
        let expected_options = options.clone();

        let handle = tokio::spawn(async move {
            sink.request_intent_confirmation("What should I work on?".into(), &options, cancel)
                .await
        });
        wait_for_pending_intent(&state).await;

        let prompt = state.intent_prompt().expect("intent prompt must be pending");
        assert_eq!(prompt.question, "What should I work on?");
        assert_eq!(prompt.options, expected_options);

        // Confirming with a selection resolves the pending prompt.
        state.resolve_intent(Some(RequestedOutcome::Execute));
        assert_eq!(handle.await.expect("intent task panicked"), Some(RequestedOutcome::Execute));
        assert!(state.intent_prompt().is_none(), "after resolve no intent should be pending");
    }

    #[tokio::test]
    async fn intent_sink_reject_resolves_to_none() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let cancel = concerto_core::CancellationToken::new();
        let options = vec![RequestedOutcome::Answer];

        let handle = tokio::spawn(async move {
            sink.request_intent_confirmation("Proceed?".into(), &options, cancel).await
        });
        wait_for_pending_intent(&state).await;

        // The Esc/q path resolves the prompt with `None` (reject).
        state.resolve_intent(None);
        assert_eq!(handle.await.expect("intent task panicked"), None);
        assert!(state.intent_prompt().is_none());
    }

    #[tokio::test]
    async fn intent_sink_returns_none_when_prompt_dropped_without_answer() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let cancel = concerto_core::CancellationToken::new();
        let options = vec![RequestedOutcome::Plan, RequestedOutcome::Execute];

        let handle = tokio::spawn(async move {
            sink.request_intent_confirmation("Plan or do?".into(), &options, cancel).await
        });
        wait_for_pending_intent(&state).await;

        // Dropping the pending prompt without resolving it cancels the wait
        // channel; the sink falls back to the conservative read-only None.
        {
            let _ = state.pending_intent.lock().ok().and_then(|mut pending| pending.take());
        }
        assert_eq!(handle.await.expect("intent task panicked"), None);
    }

    #[tokio::test]
    async fn intent_sink_empty_options_returns_none_without_prompt() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let cancel = concerto_core::CancellationToken::new();
        let options: Vec<RequestedOutcome> = Vec::new();
        // Nothing to confirm: the sink must not queue a prompt and returns
        // the conservative read-only None.
        let result =
            sink.request_intent_confirmation("nothing to confirm".into(), &options, cancel).await;
        assert_eq!(result, None);
        assert!(state.intent_prompt().is_none());
    }

    // ------------------------------------------------------------------
    // request_plan_approval (ADR-55 Phase 1d)
    // ------------------------------------------------------------------

    /// Wait until the spawned sink future has installed the pending plan so
    /// the test resolves it without racing the spawn. Bounded so a broken sink
    /// fails the test instead of hanging forever.
    async fn wait_for_pending_plan(state: &CliApprovalState) {
        for _ in 0..500 {
            if state.plan_prompt().is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("plan approval never queued a prompt");
    }

    #[tokio::test]
    async fn plan_sink_round_trip_apply() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        let plan_id = "01JTESTPLAN0000000000001A";
        let cancel = concerto_core::CancellationToken::new();

        let handle = tokio::spawn(async move {
            sink.request_plan_approval(
                session_id,
                plan_id,
                "Apply the stored plan?".into(),
                "step 1: rework module\nstep 2: verify",
                time::OffsetDateTime::now_utc(),
                cancel,
            )
            .await
        });
        wait_for_pending_plan(&state).await;

        let prompt = state.plan_prompt().expect("plan prompt must be pending");
        assert_eq!(prompt.session_id, session_id);
        assert_eq!(prompt.plan_id, plan_id);
        assert_eq!(prompt.question, "Apply the stored plan?");

        // Applying resolves the pending prompt as `Some(Apply)`.
        assert!(
            state.resolve_plan(session_id, plan_id, Some(PlanDecision::Apply)),
            "a matching pending plan must resolve"
        );
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
        assert!(state.plan_prompt().is_none(), "after resolve no plan should be pending");
    }

    #[tokio::test]
    async fn plan_sink_round_trip_replan() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        let plan_id = "01JTESTPLAN0000000000001B";
        let cancel = concerto_core::CancellationToken::new();

        let handle = tokio::spawn(async move {
            sink.request_plan_approval(
                session_id,
                plan_id,
                "Apply it or replan?".into(),
                "step 1: draft",
                time::OffsetDateTime::now_utc(),
                cancel,
            )
            .await
        });
        wait_for_pending_plan(&state).await;

        assert!(
            state.resolve_plan(session_id, plan_id, Some(PlanDecision::Replan)),
            "a matching pending plan must resolve"
        );
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
        assert!(state.plan_prompt().is_none());
    }

    #[tokio::test]
    async fn plan_sink_dismiss_resolves_to_none() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        let plan_id = "01JTESTPLAN0000000000001C";
        let cancel = concerto_core::CancellationToken::new();

        let handle = tokio::spawn(async move {
            sink.request_plan_approval(
                session_id,
                plan_id,
                "Apply the stored plan?".into(),
                "step 1: change",
                time::OffsetDateTime::now_utc(),
                cancel,
            )
            .await
        });
        wait_for_pending_plan(&state).await;

        // The Esc/q path resolves the prompt as `None` (dismiss → read-only).
        assert!(state.resolve_plan(session_id, plan_id, None), "a pending plan must resolve");
        assert_eq!(handle.await.expect("plan task panicked"), None);
        assert!(state.plan_prompt().is_none());
    }

    #[tokio::test]
    async fn plan_sink_returns_none_when_prompt_dropped_without_answer() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        let plan_id = "01JTESTPLAN0000000000001D";
        let cancel = concerto_core::CancellationToken::new();

        let handle = tokio::spawn(async move {
            sink.request_plan_approval(
                session_id,
                plan_id,
                "Apply the stored plan?".into(),
                "step 1: change",
                time::OffsetDateTime::now_utc(),
                cancel,
            )
            .await
        });
        wait_for_pending_plan(&state).await;

        // Dropping the pending prompt without resolving it cancels the wait
        // channel; the sink falls back to the conservative read-only None.
        {
            let _ = state.pending_plan.lock().ok().and_then(|mut pending| pending.take());
        }
        assert_eq!(handle.await.expect("plan task panicked"), None);
    }

    #[tokio::test]
    async fn plan_sink_occupied_slot_returns_none_conservatively() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001E";
        const SECOND_PLAN_ID: &str = "01JTESTPLAN0000000000001F";
        let cancel = concerto_core::CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                sink.request_plan_approval(
                    session_id,
                    PLAN_ID,
                    "Apply the stored plan?".into(),
                    "step 1: change",
                    time::OffsetDateTime::now_utc(),
                    cancel,
                )
                .await
            }
        });
        wait_for_pending_plan(&state).await;

        // A second plan request while one is pending gets no receiver: the
        // caller falls back to the conservative read-only None.
        let second = CliApprovalSink::with_state(state.clone())
            .request_plan_approval(
                session_id,
                SECOND_PLAN_ID,
                "Second request?".into(),
                "step 1: other change",
                time::OffsetDateTime::now_utc(),
                cancel,
            )
            .await;
        assert_eq!(second, None, "occupied slot must not install a second prompt");

        // The first prompt is unaffected and still resolves.
        assert!(state.resolve_plan(session_id, PLAN_ID, Some(PlanDecision::Apply)));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
    }

    #[tokio::test]
    async fn plan_sink_cross_session_mismatch_does_not_resolve() {
        let state = CliApprovalState::default();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        let other_session = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001E";
        let cancel = concerto_core::CancellationToken::new();

        let handle = tokio::spawn(async move {
            sink.request_plan_approval(
                session_id,
                PLAN_ID,
                "Apply the stored plan?".into(),
                "step 1: change",
                time::OffsetDateTime::now_utc(),
                cancel,
            )
            .await
        });
        wait_for_pending_plan(&state).await;

        // A mismatch on session_id must not answer this prompt: the slot stays
        // in place and no decision is delivered.
        assert!(
            !state.resolve_plan(other_session, PLAN_ID, Some(PlanDecision::Apply)),
            "a cross-session resolve must be rejected"
        );
        assert!(state.plan_prompt().is_some(), "mismatched resolve must leave the prompt");
        assert!(!handle.is_finished(), "mismatched resolve must not answer the task");

        // A mismatch on plan_id must also be rejected.
        assert!(
            !state.resolve_plan(session_id, "01JTESTPLAN0000000000FFFF", None),
            "a wrong plan_id resolve must be rejected"
        );
        assert!(state.plan_prompt().is_some());

        // The matching resolve then completes as expected.
        assert!(state.resolve_plan(session_id, PLAN_ID, Some(PlanDecision::Replan)));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
        assert!(state.plan_prompt().is_none());
    }
}
