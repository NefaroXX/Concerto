//! ADR-55 Phase 2c / ADR-56: LLM intent classifier — the primary intent
//! decider when enabled.
//!
//! When `[intent] classifier_enabled` is true (the **default**, ADR-56 §2),
//! the LLM classifier is the intent authority for every non-fast-path
//! message: the deterministic router ([`concerto_core::intent::route`]) result
//! — a rule hit, a question-detection result, or `AskUser` ambiguity — is
//! re-classified once by the LLM before the intent gate runs. A
//! classification at or above the configured confidence threshold re-routes
//! the run to the suggested outcome; anything below threshold, a malformed
//! reply, a provider failure, or a cancellation fails soft back to the
//! unchanged deterministic result (ADR-56 §3/§4).
//!
//! Contract (ADR-55 Phase 2c §1–§6; ADR-56 §1/§3/§4/§5):
//!
//! - Mounted after the two deterministic fast paths: the caller skips this
//!   module for negation-override and smalltalk routes, which win outright
//!   and never reach the provider — and every other route (rule hits,
//!   questions, and `AskUser` alike) is classified when enabled (ADR-56 §1).
//! - **One bounded non-streaming provider call** through the normal provider
//!   stack, collected under the run's time-to-first-byte / stream-idle
//!   timeouts (§3, §6). No additional retry loop beyond that single call.
//! - **Reserve-before-call spend**: [`SpendTracker::check_and_add`] gates the
//!   call (a cap-exceeded reservation skips it — the deterministic result
//!   stands, §6/C5) and [`SpendTracker::settle_reservation`] records actual
//!   spend afterwards. The run's per-session spend carry-forward is recorded
//!   by the caller *before* this module runs (§6 ordering requirement), so a
//!   session already over cap cannot fire a classifier call.
//! - **Audit** (§5, C4): every invocation writes exactly one `intent_router`
//!   row with `rule_matched = "llm_classifier"`, `verdict = "n/a"`, and a JSON
//!   envelope `{"route", "confidence", "threshold", "rationale"}` in
//!   `user_response`. The classifier-created [`Ulid`] is returned to the
//!   caller so the router's own row for the same event shares the correlation
//!   id.
//! - **Never grants** (§4): the classifier classifies; [`apply_classifier_decision`]
//!   only re-routes, and the caller sends the result through the exact
//!   confirmation machinery — a re-routed Execute still requires the user's
//!   confirmation dialog.
//!
//! Audit-row scope (documented interpretation): an *invocation* means the
//! provider call fired. When the call never fires — classifier disabled, no
//! model, token already cancelled at entry, or a failed spend reservation —
//! no row is written (there is no model output to record) and the
//! deterministic routing result stands.

use std::sync::Arc;
use std::time::Duration;

use concerto_config::AppConfig;
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::intent::{RequestedOutcome, RouterOutput, RouterRoute};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{CompletionRequest, Message, Role};
use concerto_core::{CancellationToken, SpendTracker};
use tracing::{debug, warn};

use crate::intent_grants::outcome_name;
use crate::prompts::{collect_stream_with_timeouts, parse_json_substring};

/// Upper bound on the rationale carried in the audit envelope (§5: ≤512 chars).
const MAX_RATIONALE_CHARS: usize = 512;

/// Upper bound on the classifier's JSON reply (one tiny object).
const MAX_CLASSIFIER_OUTPUT_TOKENS: u64 = 200;

/// Fixed prompt-size budget used for the reserve-before-call spend estimate
/// (§6). The one-shot prompt and reply are far below any cap; the reserve
/// still fails cleanly when a session is already at/over cap.
const CLASSIFIER_INPUT_TOKENS: u64 = 1_000;
const CLASSIFIER_OUTPUT_TOKENS: u64 = 250;

/// A single classified outcome produced by the LLM classifier.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifyOutcome {
    /// The suggested outcome, restricted to the six-outcome set
    /// [`RequestedOutcome`] (anything else is a parse rejection → fail-soft).
    pub outcome: RequestedOutcome,
    /// The model's confidence, clamped to `0.0..=1.0` (path selection only).
    pub confidence: f32,
    /// The model's rationale, truncated to [`MAX_RATIONALE_CHARS`] chars.
    pub rationale: String,
}

/// Everything [`classify_ambiguity`] needs from the calling run.
pub struct ClassifierContext<'a> {
    /// Resolved application config (the `[intent]` section).
    pub config: &'a AppConfig,
    /// The run's provider — the classifier goes through the normal stack.
    pub provider: &'a Arc<dyn LlmProvider>,
    /// The run's chat model — fallback when `classifier_model` is unset (§2).
    pub run_model: &'a str,
    /// Audit channel for the classifier row.
    pub executor: &'a ToolExecutor,
    /// Spend tracker for reserve-before-call accounting.
    pub spend_tracker: &'a SpendTracker,
    /// The session being run (spend + audit attribution).
    pub session_id: Ulid,
    /// The raw user request, echoed in the prompt and the audit row.
    pub utterance: &'a str,
    pub cancel: CancellationToken,
}

/// The result of one classifier invocation.
#[derive(Debug, Clone)]
pub struct ClassifierCall {
    /// Correlation id created at classifier start; the caller reuses it for
    /// the router-decision row of the same event (§5).
    pub correlation_id: Ulid,
    /// The configured threshold the classifier applied (validated
    /// `>= LOW_CONFIDENCE_THRESHOLD` at config load, §2).
    pub threshold: f32,
    /// `Some` = the model produced a valid classification (any confidence;
    /// whether it re-routes is the caller's [`should_reroute`] decision).
    /// `None` = fail-soft: the run stays on AskUser unchanged.
    pub outcome: Option<ClassifyOutcome>,
}

/// Run the LLM intent classifier for one non-fast-path request (ADR-56 §1).
///
/// Returns `None` when the classifier is disabled or unavailable (no
/// invocation, no audit row). Returns `Some(ClassifierCall)` for every
/// attempted invocation; `call.outcome` is `None` on fail-soft (below
/// threshold is **not** fail-soft — the suggestion is returned and the
/// caller decides whether to re-route).
pub async fn classify_ambiguity(ctx: ClassifierContext<'_>) -> Option<ClassifierCall> {
    let Some(intent) = ctx.config.intent.as_ref() else {
        debug!("intent classifier disabled: no [intent] section configured");
        return None;
    };
    if !intent.classifier_enabled {
        debug!("intent classifier disabled: classifier_enabled = false");
        return None;
    }
    if ctx.cancel.is_cancelled() {
        debug!("intent classifier skipped: run already cancelled");
        return None;
    }

    let threshold = intent.classifier_confidence_threshold;
    let model = classifier_model(intent, ctx.run_model);
    let correlation_id = Ulid::new();

    // Reserve-before-call (§6): a cap-exceeded reservation means the call
    // never happens and AskUser stands.
    let estimated =
        ctx.provider.approximate_cost(CLASSIFIER_INPUT_TOKENS, CLASSIFIER_OUTPUT_TOKENS);
    if let Err(error) = ctx.spend_tracker.check_and_add(estimated) {
        warn!(%error, "intent classifier spend reservation failed; skipping classifier call (fail-soft)");
        return Some(ClassifierCall { correlation_id, threshold, outcome: None });
    }

    // One bounded non-streaming call (§3): a single logical request through
    // the provider stack, collected under the run's ttfb / stream-idle
    // timeouts. No additional retry loop — a failure fails soft.
    let request = CompletionRequest {
        model,
        messages: classifier_messages(ctx.utterance),
        tools: None,
        tool_choice: None,
        temperature: Some(0.0),
        max_tokens: Some(MAX_CLASSIFIER_OUTPUT_TOKENS),
        stream: true,
    };
    let cancel = ctx.cancel.clone();
    let first_byte_timeout = Duration::from_secs(ctx.config.retry.time_to_first_byte_seconds);
    let idle_timeout = Duration::from_secs(ctx.config.retry.stream_idle_timeout_seconds);
    let collected = match ctx.provider.stream_completion(request, cancel.clone()).await {
        Ok(stream) => {
            collect_stream_with_timeouts(stream, &cancel, first_byte_timeout, idle_timeout).await
        }
        Err(error) => Err(error),
    };

    match collected {
        Ok((text, _, _, usage)) => {
            // Settle with the provider-reported usage when present; fall back
            // to the reservation otherwise (measured beats estimate). A usage
            // struct with both counts unknown is treated as absent.
            let actual = usage
                .filter(|usage| usage.prompt_tokens.is_some() || usage.completion_tokens.is_some())
                .map_or(estimated, |usage| {
                    ctx.provider.approximate_cost(
                        usage.prompt_tokens.unwrap_or(0),
                        usage.completion_tokens.unwrap_or(0),
                    )
                });
            ctx.spend_tracker.settle_reservation(estimated, actual);
            let outcome = parse_classifier_reply(&text);
            let call = ClassifierCall { correlation_id, threshold, outcome };
            write_classifier_audit(&ctx, &call).await;
            debug!(
                %correlation_id,
                parsed = call.outcome.is_some(),
                "intent classifier call completed"
            );
            Some(call)
        }
        Err(error) => {
            // Refund the reserve on failure (repo convention: a failed call
            // produced no output, so no spend is attributed — mirrors
            // `agent_runner` settling a failed delegation with 0.0).
            ctx.spend_tracker.settle_reservation(estimated, 0.0);
            warn!(%error, "intent classifier provider call failed; fail-soft to the deterministic route");
            let call = ClassifierCall { correlation_id, threshold, outcome: None };
            write_classifier_audit(&ctx, &call).await;
            Some(call)
        }
    }
}

/// Path-selection decision (§3/§4): re-route the deterministic result only
/// when the classified confidence is at or above the configured threshold. The
/// threshold is validated `>= LOW_CONFIDENCE_THRESHOLD` at config load, so a
/// re-route always satisfies the gate's arm-1 dialog predicate too — no
/// configured threshold can create a `[threshold, LOW_CONFIDENCE_THRESHOLD)`
/// band (§2).
pub fn should_reroute(outcome: &ClassifyOutcome, threshold: f32) -> bool {
    outcome.confidence >= threshold
}

/// Apply a confident classifier suggestion to the routing output (§3/§4).
///
/// Replaces the deterministic route and its confidence with the classified
/// outcome + confidence so the gate (`bound_plan_for_approval` /
/// `apply_intent_gate`) treats a re-routed Execute exactly like a
/// deterministic confident Execute — including the arm-1 confirmation dialog
/// (the classifier classifies, never grants). Returns whether a re-route
/// happened. The caller keeps the pre-replacement route name (captured before
/// the classifier call) for the router-decision audit row (§5).
pub fn apply_classifier_decision(routing: &mut RouterOutput, call: &ClassifierCall) -> bool {
    let Some(outcome) = call.outcome.as_ref() else {
        return false;
    };
    if !should_reroute(outcome, call.threshold) {
        return false;
    }
    routing.outcome = outcome.outcome;
    routing.confidence = outcome.confidence;
    routing.route = RouterRoute::LlmClassifier;
    true
}

/// The effective classifier model: the configured `classifier_model` when set
/// and non-empty, else the run's chat model (§2, §9).
fn classifier_model(intent: &concerto_config::IntentConfig, run_model: &str) -> String {
    intent
        .classifier_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| run_model.to_owned())
}

/// The one-shot prompt: a system instruction demanding a single JSON object
/// plus the raw utterance. Output is additionally bounded by `max_tokens` in
/// the request.
fn classifier_messages(utterance: &str) -> Vec<Message> {
    let system = "You classify a coding-agent user request into exactly one of six outcomes. \
                  Reply with a single JSON object only — no markdown, no prose, no trailing \
                  text: {\"route\": \"<outcome>\", \"confidence\": <0.0..1.0>, \"rationale\": \
                  \"<short reason>\"}. Outcomes: \"answer\" (a text answer; no tool use), \
                  \"diagnose\" (investigate why something is broken and explain it), \
                  \"review\" (read-only critique of existing code), \"plan\" (produce a plan \
                  or design as text; no writes), \"execute\" (implement or change code — the \
                  mutation path), \"verify\" (run tests or checks). When uncertain, prefer \
                  the most conservative outcome (answer or review over execute) and reflect \
                  the uncertainty in a lower confidence.";
    vec![
        Message {
            role: Role::System,
            content: system.to_owned(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        },
        Message {
            role: Role::User,
            content: utterance.to_owned(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        },
    ]
}

/// Strict parser for the classifier's `{route, confidence, rationale}` reply.
///
/// Reuses the codebase's model-reply JSON extraction ([`parse_json_substring`]
/// — accepts strict, fenced, and prose-surrounded JSON) and then applies the
/// ADR's strictness rules: `route` must deserialize to one of the six
/// [`RequestedOutcome`] variants, unknown fields are rejected, confidence must
/// be finite and is clamped to `0.0..=1.0`, and the rationale is truncated to
/// 512 chars. Any deviation returns `None` (fail-soft).
fn parse_classifier_reply(text: &str) -> Option<ClassifyOutcome> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ClassifierReply {
        route: RequestedOutcome,
        confidence: f32,
        rationale: String,
    }

    let reply: ClassifierReply = parse_json_substring(text)?;
    let confidence = reply.confidence;
    if !confidence.is_finite() {
        return None;
    }
    let confidence = confidence.clamp(0.0, 1.0);
    let rationale = reply.rationale.chars().take(MAX_RATIONALE_CHARS).collect();
    Some(ClassifyOutcome { outcome: reply.route, confidence, rationale })
}

/// Write the classifier's audit row (§5): `intent_router` channel,
/// `rule_matched = "llm_classifier"`, `verdict = "n/a"`, and the JSON envelope
/// in `user_response`. Fail-soft rows (below-threshold, malformed,
/// provider-error, cancelled, cap-exceeded) carry zero confidence and the
/// literal envelope route `"ask_user"` — a schema-stable marker meaning "no
/// classification to report". The actual pre-replacement route name is NOT
/// known here: the caller captures it before the classifier call and records
/// it on the router-decision row (ADR-56 §5), so this row never fabricates
/// that name.
async fn write_classifier_audit(ctx: &ClassifierContext<'_>, call: &ClassifierCall) {
    let confidence = call.outcome.as_ref().map_or(0.0, |outcome| outcome.confidence);
    let envelope = classifier_envelope(call.outcome.as_ref(), call.threshold);
    ctx.executor
        .record_routing_decision(
            ctx.session_id,
            call.correlation_id,
            ctx.utterance,
            "llm_classifier",
            &envelope,
            confidence,
            "n/a",
            ctx.cancel.clone(),
        )
        .await;
}

/// The audit envelope for a classifier invocation (§5):
/// `{"route", "confidence", "threshold", "rationale"}`. On fail-soft the route
/// is the literal `"ask_user"` marker with zero confidence and an empty
/// rationale — the envelope never fabricates a classification, and the actual
/// pre-replacement route name is recorded by the caller's router-decision row
/// via the pre-captured `router_route` (ADR-56 §5).
fn classifier_envelope(outcome: Option<&ClassifyOutcome>, threshold: f32) -> String {
    let (route, confidence, rationale) = match outcome {
        Some(outcome) => {
            (outcome_name(outcome.outcome), outcome.confidence, outcome.rationale.as_str())
        }
        None => ("ask_user", 0.0, ""),
    };
    serde_json::json!({
        "route": route,
        "confidence": confidence,
        "threshold": threshold,
        "rationale": rationale,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use concerto_config::IntentConfig;
    use concerto_core::error::ProviderError;
    use concerto_core::intent::LOW_CONFIDENCE_THRESHOLD;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::traits::provider::CompletionStream;
    use concerto_core::types::{CompletionChunk, CompletionUsage, TokenBudget, ToolRegistry};
    use concerto_core::SimplePolicyEngine;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A scripted provider: each `stream_completion` call pops the next queued
    /// reply (raw text delivered as one final chunk) or fails with the queued
    /// error. Records the call count and the request model names seen.
    struct StubProvider {
        replies: Mutex<VecDeque<Result<String, ProviderError>>>,
        calls: AtomicUsize,
        models: Mutex<Vec<String>>,
    }

    impl StubProvider {
        fn new(replies: Vec<Result<String, ProviderError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: AtomicUsize::new(0),
                models: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn models(&self) -> Vec<String> {
            self.models.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn provider_name(&self) -> &'static str {
            "stub-classifier"
        }
        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            request: CompletionRequest,
            cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.models.lock().unwrap_or_else(|e| e.into_inner()).push(request.model);
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let reply = self
                .replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()));
            match reply {
                Ok(text) => {
                    let chunk = CompletionChunk {
                        reasoning: None,
                        delta: text,
                        tool_call: None,
                        is_final: true,
                        usage: Some(CompletionUsage {
                            prompt_tokens: Some(900),
                            completion_tokens: Some(50),
                        }),
                    };
                    Ok(Box::pin(futures::stream::iter(std::iter::once(Ok(chunk)))))
                }
                Err(error) => Err(error),
            }
        }
    }

    /// Captures every audit entry so tests can assert the classifier row's
    /// rule / envelope / correlation id (§5, C4).
    #[derive(Clone)]
    struct CapturingAudit(Arc<Mutex<Vec<AuditEntry>>>);

    #[async_trait]
    impl AuditLog for CapturingAudit {
        async fn record(
            &self,
            entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
            Ok(())
        }
    }

    impl CapturingAudit {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }

        fn entries(&self) -> Vec<AuditEntry> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    fn classifier_config(enabled: bool, model: Option<&str>, threshold: f32) -> AppConfig {
        AppConfig {
            intent: Some(IntentConfig {
                classifier_enabled: enabled,
                classifier_model: model.map(str::to_owned),
                classifier_confidence_threshold: threshold,
            }),
            ..AppConfig::default()
        }
    }

    fn make_executor(audit: CapturingAudit) -> Arc<ToolExecutor> {
        let policy = Arc::new(SimplePolicyEngine::new(Vec::new(), Arc::new(audit)));
        Arc::new(ToolExecutor::new(Arc::new(ToolRegistry::default()), policy))
    }

    #[allow(clippy::too_many_arguments)]
    fn context<'a>(
        config: &'a AppConfig,
        provider: &'a Arc<dyn LlmProvider>,
        executor: &'a ToolExecutor,
        spend: &'a SpendTracker,
        session_id: Ulid,
        utterance: &'a str,
        cancel: CancellationToken,
    ) -> ClassifierContext<'a> {
        ClassifierContext {
            config,
            provider,
            run_model: "run-chat-model",
            executor,
            spend_tracker: spend,
            session_id,
            utterance,
            cancel,
        }
    }

    #[tokio::test]
    async fn missing_intent_section_skips_the_classifier() {
        // A config with NO `[intent]` section has no classifier config at all —
        // the deterministic router is the only routing path (ADR-56 §3): no
        // provider call, no invocation, no audit row.
        let config = AppConfig::default();
        let stub = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.95,"rationale":"clear request"}"#.into(),
        )]));
        let provider: Arc<dyn LlmProvider> = stub.clone();
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await;

        assert!(call.is_none(), "a config without [intent] never invokes the classifier");
        assert_eq!(stub.calls(), 0, "no provider call without a [intent] section");
        assert!(audit.entries().is_empty(), "no audit row without an invocation");
    }

    /// ADR-56 §2: `[intent] classifier_enabled` now DEFAULTS to true — the
    /// defaulted section inserted by `migrate_v6_to_v7` (no explicit
    /// `classifier_enabled` key) invokes the classifier for a non-fast-path
    /// utterance instead of skipping it.
    #[tokio::test]
    async fn classifier_runs_by_default_with_defaulted_intent_section() {
        let config = AppConfig { intent: Some(IntentConfig::default()), ..AppConfig::default() };
        let stub = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.95,"rationale":"clear request"}"#.into(),
        )]));
        let provider: Arc<dyn LlmProvider> = stub.clone();
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("a defaulted [intent] section must invoke the classifier");

        assert!(call.outcome.is_some(), "the default-on classifier parses the reply");
        assert_eq!(stub.calls(), 1, "default-on classifier makes exactly one provider call");
        assert_eq!(audit.entries().len(), 1, "an invocation writes one classifier row");
    }

    #[tokio::test]
    async fn above_threshold_classification_is_returned_for_reroute() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.92,"rationale":"mutate the parser"}"#.into(),
        )]));
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());
        let spend = SpendTracker::new(None, None, None);

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &spend,
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("a valid reply yields a call");

        let outcome = call.outcome.as_ref().expect("a valid reply yields an outcome");
        assert_eq!(outcome.outcome, RequestedOutcome::Execute);
        assert_eq!(outcome.confidence, 0.92);
        assert_eq!(outcome.rationale, "mutate the parser");
        assert!(should_reroute(outcome, call.threshold), "confidence >= threshold must re-route");

        // §5/C4: one classifier row, correct fields, correlation id matches.
        let entries = audit.entries();
        assert_eq!(entries.len(), 1, "exactly one audit row per invocation");
        let row = &entries[0];
        assert_eq!(row.tool_name, "intent_router");
        assert_eq!(row.rule_matched.as_deref(), Some("llm_classifier"));
        assert_eq!(row.verdict, "n/a", "no confirmation was solicited");
        assert_eq!(row.correlation_id, call.correlation_id, "shared correlation id");
        let envelope: serde_json::Value =
            serde_json::from_str(row.user_response.as_deref().expect("envelope in user_response"))
                .expect("envelope is valid JSON");
        assert_eq!(envelope["route"], "Execute");
        let confidence = envelope["confidence"].as_f64().expect("confidence is a number");
        assert!((confidence - 0.92).abs() < 1e-6, "envelope confidence mismatch: {confidence}");
        let threshold = envelope["threshold"].as_f64().expect("threshold is a number");
        assert!((threshold - 0.7).abs() < 1e-6, "envelope threshold mismatch: {threshold}");
        assert_eq!(envelope["rationale"], "mutate the parser");
    }

    /// ADR-55 Phase 2c §5/C4 — the runtime_runner wiring records TWO
    /// `intent_router` rows for one classifier-eligible event: the classifier's
    /// own row (`rule_matched = "llm_classifier"`, `verdict = "n/a"`) plus the
    /// router's row carrying the PRE-classifier route name (`ask_user`) and the
    /// SAME correlation id. `run_shared_agent` is not drivable in unit tests
    /// (config-resolved provider, durable audit), so this test drives the exact
    /// two calls the wiring makes — `classify_ambiguity` followed by
    /// `record_routing_decision` with the shared correlation id and the
    /// pre-classifier route name — and asserts the chain (re-routed case).
    #[tokio::test]
    async fn classifier_event_records_two_row_audit_chain() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.92,"rationale":"mutate the parser"}"#.into(),
        )]));
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());
        let session_id = Ulid::new();
        let utterance = "hello there";

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            session_id,
            utterance,
            CancellationToken::new(),
        ))
        .await
        .expect("a valid reply yields a call");

        // Router row — the two inputs the runtime_runner wiring supplies: the
        // shared correlation id and the PRE-classifier route name (§5/C4).
        executor
            .record_routing_decision(
                session_id,
                call.correlation_id,
                utterance,
                "ask_user",
                "Execute",
                0.92,
                "n/a",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries();
        assert_eq!(entries.len(), 2, "exactly two intent_router rows per classifier event");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("llm_classifier"));
        assert_eq!(entries[0].verdict, "n/a", "classifier solicits no confirmation");
        assert_eq!(entries[1].rule_matched.as_deref(), Some("ask_user"));
        assert_eq!(entries[0].correlation_id, call.correlation_id, "shared correlation id");
        assert_eq!(entries[1].correlation_id, call.correlation_id, "shared correlation id");
    }

    /// Same two-row chain, fail-soft case: a malformed classifier reply yields
    /// `outcome: None`, AskUser stands, and the chain still records both rows
    /// under one correlation id.
    #[tokio::test]
    async fn classifier_event_fail_soft_records_two_row_chain() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> =
            Arc::new(StubProvider::new(vec![Ok("this is not valid json at all".into())]));
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());
        let session_id = Ulid::new();
        let utterance = "hello there";

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            session_id,
            utterance,
            CancellationToken::new(),
        ))
        .await
        .expect("an attempted invocation always yields a call");

        assert!(call.outcome.is_none(), "malformed reply fails soft to AskUser");

        executor
            .record_routing_decision(
                session_id,
                call.correlation_id,
                utterance,
                "ask_user",
                "AskUser",
                0.0,
                "n/a",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries();
        assert_eq!(entries.len(), 2, "fail-soft still records the two-row chain");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("llm_classifier"));
        assert_eq!(entries[0].verdict, "n/a");
        assert_eq!(entries[1].rule_matched.as_deref(), Some("ask_user"));
        assert_eq!(entries[0].correlation_id, call.correlation_id, "shared correlation id");
        assert_eq!(entries[1].correlation_id, call.correlation_id, "shared correlation id");
    }

    #[tokio::test]
    async fn below_threshold_suggestion_is_recorded_but_never_reroutes() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.4,"rationale":"weak signal"}"#.into(),
        )]));
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("a valid reply yields a call");

        let outcome = call.outcome.as_ref().expect("a valid reply yields an outcome");
        assert!(
            !should_reroute(outcome, call.threshold),
            "confidence below threshold must not re-route"
        );
        assert_eq!(audit.entries().len(), 1, "the suggestion is still audited");
    }

    #[test]
    fn threshold_boundary_is_inclusive_at_low_confidence_threshold() {
        let outcome = ClassifyOutcome {
            outcome: RequestedOutcome::Execute,
            confidence: 0.7,
            rationale: String::new(),
        };
        assert!(should_reroute(&outcome, LOW_CONFIDENCE_THRESHOLD));
        let below = ClassifyOutcome {
            outcome: RequestedOutcome::Execute,
            confidence: 0.699,
            rationale: String::new(),
        };
        assert!(!should_reroute(&below, LOW_CONFIDENCE_THRESHOLD));
    }

    #[test]
    fn apply_classifier_decision_reroutes_only_above_threshold() {
        let reroute = ClassifierCall {
            correlation_id: Ulid::new(),
            threshold: LOW_CONFIDENCE_THRESHOLD,
            outcome: Some(ClassifyOutcome {
                outcome: RequestedOutcome::Execute,
                confidence: 0.95,
                rationale: "clear".into(),
            }),
        };
        // "hmm" is genuinely ambiguous — greetings like "hello there" now
        // route to the smalltalk rule, so they no longer exercise AskUser.
        let mut routing = concerto_core::intent::route("hmm", std::path::PathBuf::new());
        assert!(matches!(routing.route, RouterRoute::AskUser));
        assert_eq!(routing.confidence, 0.0);

        assert!(apply_classifier_decision(&mut routing, &reroute));
        assert_eq!(routing.route, RouterRoute::LlmClassifier);
        assert_eq!(routing.outcome, RequestedOutcome::Execute);
        assert_eq!(routing.confidence, 0.95);

        // Below threshold: nothing changes (AskUser stands).
        let weak = ClassifierCall {
            correlation_id: Ulid::new(),
            threshold: LOW_CONFIDENCE_THRESHOLD,
            outcome: Some(ClassifyOutcome {
                outcome: RequestedOutcome::Execute,
                confidence: 0.4,
                rationale: "weak".into(),
            }),
        };
        let mut unchanged = concerto_core::intent::route("hmm", std::path::PathBuf::new());
        assert!(!apply_classifier_decision(&mut unchanged, &weak));
        assert!(matches!(unchanged.route, RouterRoute::AskUser));
        assert_eq!(unchanged.confidence, 0.0);

        // Fail-soft call: nothing changes.
        let fail_soft = ClassifierCall {
            correlation_id: Ulid::new(),
            threshold: LOW_CONFIDENCE_THRESHOLD,
            outcome: None,
        };
        assert!(!apply_classifier_decision(&mut unchanged, &fail_soft));
        assert!(matches!(unchanged.route, RouterRoute::AskUser));
    }

    #[tokio::test]
    async fn malformed_json_fails_soft_with_audit_row() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> =
            Arc::new(StubProvider::new(vec![Ok("sorry, I cannot do that".into())]));
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("an invocation still yields a call");

        assert!(call.outcome.is_none(), "unparseable output fails soft");
        let entries = audit.entries();
        assert_eq!(entries.len(), 1, "an invocation writes an audit row even on failure");
        let envelope: serde_json::Value = serde_json::from_str(
            entries[0].user_response.as_deref().expect("envelope in user_response"),
        )
        .expect("envelope is valid JSON");
        assert_eq!(envelope["route"], "ask_user", "fail-soft envelope names the ask path");
        assert_eq!(envelope["confidence"], 0.0);
    }

    #[tokio::test]
    async fn route_outside_six_outcome_set_is_rejected() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"paint","confidence":0.9,"rationale":"not an outcome"}"#.into(),
        )]));
        let executor = make_executor(CapturingAudit::new());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("an invocation still yields a call");

        assert!(
            call.outcome.is_none(),
            "a route outside the six-outcome set is a parse failure (fail-soft)"
        );
    }

    #[tokio::test]
    async fn provider_error_fails_soft_with_audit_row() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> =
            Arc::new(StubProvider::new(vec![Err(ProviderError::NotConfigured)]));
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("a provider failure still yields a call");

        assert!(call.outcome.is_none(), "provider error fails soft");
        assert_eq!(audit.entries().len(), 1, "an invocation writes an audit row even on failure");
    }

    #[tokio::test]
    async fn cancellation_before_call_skips_invocation() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let stub = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.95,"rationale":"n/a"}"#.into(),
        )]));
        let provider: Arc<dyn LlmProvider> = stub.clone();
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            cancel,
        ))
        .await;

        assert!(call.is_none(), "an already-cancelled run never invokes the classifier");
        assert_eq!(stub.calls(), 0, "no provider call on an already-cancelled run");
        assert!(audit.entries().is_empty(), "no invocation, no audit row");
    }

    #[tokio::test]
    async fn spend_cap_exceeded_skips_the_call_and_stands_ask_user() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let stub = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"execute","confidence":0.95,"rationale":"n/a"}"#.into(),
        )]));
        let provider: Arc<dyn LlmProvider> = stub.clone();
        let audit = CapturingAudit::new();
        let executor = make_executor(audit.clone());
        // Session already over its cap (record does not re-check caps; the
        // reserve does): the classifier call must never fire (§6/C5).
        let spend = SpendTracker::new(Some(0.5), None, None);
        spend.record(1.0);

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &spend,
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("a failed reservation still yields a call");

        assert!(call.outcome.is_none(), "a cap-exceeded reservation fails soft");
        assert_eq!(stub.calls(), 0, "the call never fires when the reserve fails");
        assert!(audit.entries().is_empty(), "no invocation, no audit row");
    }

    #[tokio::test]
    async fn confidence_is_clamped_to_the_unit_interval() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![
            Ok(r#"{"route":"execute","confidence":1.7,"rationale":"too high"}"#.into()),
            Ok(r#"{"route":"answer","confidence":-0.5,"rationale":"too low"}"#.into()),
        ]));
        let executor = make_executor(CapturingAudit::new());
        let spend = SpendTracker::new(None, None, None);

        let first = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &spend,
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("call");
        let second = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &spend,
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("call");

        assert_eq!(first.outcome.as_ref().expect("outcome").confidence, 1.0);
        assert_eq!(second.outcome.as_ref().expect("outcome").confidence, 0.0);
    }

    #[tokio::test]
    async fn rationale_is_truncated_to_512_chars() {
        let long_rationale = "r".repeat(600);
        let reply =
            format!(r#"{{"route":"review","confidence":0.8,"rationale":"{long_rationale}"}}"#);
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![Ok(reply)]));
        let executor = make_executor(CapturingAudit::new());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("call");

        let outcome = call.outcome.as_ref().expect("outcome");
        assert_eq!(outcome.rationale.len(), 512, "rationale is truncated to 512 chars");
    }

    #[tokio::test]
    async fn fenced_json_reply_is_accepted() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider::new(vec![Ok(
            "Here you go:\n```json\n{\"route\":\"review\",\"confidence\":0.85,\"rationale\":\"read-only critique\"}\n```"
                .into(),
        )]));
        let executor = make_executor(CapturingAudit::new());

        let call = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await
        .expect("call");

        let outcome = call.outcome.as_ref().expect("a fenced JSON reply parses");
        assert_eq!(outcome.outcome, RequestedOutcome::Review);
        assert_eq!(outcome.confidence, 0.85);
    }

    #[tokio::test]
    async fn configured_classifier_model_overrides_the_run_model() {
        let config = classifier_config(true, Some("classifier-model-v2"), LOW_CONFIDENCE_THRESHOLD);
        let stub = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"answer","confidence":0.9,"rationale":"ok"}"#.into(),
        )]));
        let provider: Arc<dyn LlmProvider> = stub.clone();
        let executor = make_executor(CapturingAudit::new());

        let _ = classify_ambiguity(context(
            &config,
            &provider,
            &executor,
            &SpendTracker::new(None, None, None),
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        ))
        .await;

        assert_eq!(stub.models(), vec!["classifier-model-v2".to_owned()]);
    }

    #[tokio::test]
    async fn run_model_is_used_when_classifier_model_is_unset() {
        let config = classifier_config(true, None, LOW_CONFIDENCE_THRESHOLD);
        let stub = Arc::new(StubProvider::new(vec![Ok(
            r#"{"route":"answer","confidence":0.9,"rationale":"ok"}"#.into(),
        )]));
        let provider: Arc<dyn LlmProvider> = stub.clone();
        let executor = make_executor(CapturingAudit::new());
        let spend = SpendTracker::new(None, None, None);

        let mut ctx = context(
            &config,
            &provider,
            &executor,
            &spend,
            Ulid::new(),
            "hello there",
            CancellationToken::new(),
        );
        ctx.run_model = "run-chat-model";

        let _ = classify_ambiguity(ctx).await;

        assert_eq!(stub.models(), vec!["run-chat-model".to_owned()]);
    }

    #[test]
    fn parse_rejects_unknown_fields_and_missing_confidence() {
        let unknown = parse_classifier_reply(
            r#"{"route":"execute","confidence":0.9,"rationale":"ok","extra":"nope"}"#,
        );
        assert!(unknown.is_none(), "unknown envelope fields are rejected (strict parse)");
        let missing = parse_classifier_reply(r#"{"route":"execute","rationale":"ok"}"#);
        assert!(missing.is_none(), "a missing confidence is a parse failure");
    }
}
