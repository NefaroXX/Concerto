//! Deterministic, no-LLM-in-loop context assembly (ADR-048).
//!
//! The engine winds the additive `[context]` config surface onto the existing
//! deterministic checkpoint machinery in [`crate::context_compaction`]:
//!
//! * [`ContextEngine::plan`] is a deterministic planner over an in-memory
//!   message window. It never touches the store, the model, or the transcript,
//!   and it decides whether the window warrants structural compaction before a
//!   request is assembled.
//! * [`ContextEngine::assemble`] runs the durable compaction pass
//!   (checkpoint summaries + a bounded recent tail) with the configured policy
//!   and returns the bounded history for the next model request.
//! * [`ContextEngine::maintain`] checkpoints any newly eligible ranges after a
//!   completed run.
//!
//! Defaults are the existing behavior (`trigger_tokens` 16000,
//! `retain_user_turns` 4, `minimum_user_turns` 6), so a config without a
//! `[context]` section — or with only some knobs set — is byte-identical to
//! today's runtime.

use std::sync::Arc;

use concerto_config::ContextConfig;
use concerto_core::ids::Ulid;
use concerto_core::types::Message;
use concerto_core::CancellationToken;
use concerto_sessions::{SessionError, SessionStore};

use crate::context_compaction::{
    self, estimate_messages_tokens, plan_compaction, CompactionPolicy,
};

/// The budget the engine resolves from `[context]`. Mirrors the embedded
/// compaction defaults so an unset knob keeps today's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudgetPolicy {
    /// Estimated tokens before deterministic compaction triggers (default
    /// 16000).
    pub trigger_tokens: u64,
    /// Most-recent user turns always retained verbatim after compaction
    /// (default 4).
    pub retain_user_turns: usize,
    /// Minimum user turns before compaction may fire (default 6).
    pub minimum_user_turns: usize,
}

impl Default for ContextBudgetPolicy {
    fn default() -> Self {
        Self { trigger_tokens: 16_000, retain_user_turns: 4, minimum_user_turns: 6 }
    }
}

impl ContextBudgetPolicy {
    /// Resolve from the additive `[context]` config surface. `None`, or an
    /// unset knob, keeps the default.
    pub fn from_config(config: Option<&ContextConfig>) -> Self {
        let default = Self::default();
        match config {
            None => default,
            Some(context) => Self {
                trigger_tokens: context.trigger_tokens.unwrap_or(default.trigger_tokens),
                retain_user_turns: context.retain_user_turns.unwrap_or(default.retain_user_turns),
                minimum_user_turns: context
                    .minimum_user_turns
                    .unwrap_or(default.minimum_user_turns),
            },
        }
    }
}

/// The planner's verdict for a message window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPlan {
    /// Estimate is within the soft budget (or user turns below the minimum);
    /// pass the window through byte-identical.
    PassThrough { estimated_tokens: u64 },
    /// Estimate exceeds the trigger; deterministic compaction should run
    /// before the request is assembled.
    Compact { estimated_tokens: u64 },
}

/// ContextEngine v2 — deterministic, byte-stable context assembly.
#[derive(Debug, Clone, Copy)]
pub struct ContextEngine {
    budget: ContextBudgetPolicy,
}

impl ContextEngine {
    /// Engine with the ADR-048 defaults (trigger 16000 / retain 4 / minimum
    /// 6) — identical to today's runtime.
    pub fn new() -> Self {
        Self { budget: ContextBudgetPolicy::default() }
    }

    /// Resolve the engine from the additive `[context]` config surface.
    pub fn from_config(config: Option<&ContextConfig>) -> Self {
        Self { budget: ContextBudgetPolicy::from_config(config) }
    }

    /// The resolved budget policy (exposed for tests and diagnostics).
    pub fn budget(&self) -> ContextBudgetPolicy {
        self.budget
    }

    /// Deterministic planning stage (ADR-048 §2a): decide whether the given
    /// window warrants structural compaction. Pure and store-free.
    pub fn plan(&self, messages: &[Message]) -> ContextPlan {
        match plan_compaction(messages, self.compaction_policy()) {
            crate::context_compaction::CompactionAdvice::PassThrough { estimated_tokens } => {
                ContextPlan::PassThrough { estimated_tokens }
            }
            crate::context_compaction::CompactionAdvice::Compact { estimated_tokens } => {
                ContextPlan::Compact { estimated_tokens }
            }
        }
    }

    /// Token estimate for a window, using the shared production estimator
    /// (`bytes / 4` + per-message overhead).
    pub fn estimate_tokens(&self, messages: &[Message]) -> u64 {
        estimate_messages_tokens(messages)
    }

    /// Assemble the bounded active history for the next request: maintain any
    /// newly eligible durable checkpoints under this engine's policy, then
    /// materialize the checkpoint frontier plus the uncompacted tail. The
    /// source transcript is never modified. When a bus is supplied, every
    /// compaction decision is published as `EventKind::ContextCompacted`
    /// (ADR-048 §5).
    pub async fn assemble(
        &self,
        store: Arc<dyn SessionStore>,
        session_id: Ulid,
        fallback_history: &[Message],
        cancel: CancellationToken,
        bus: Option<&concerto_core::event::EventBus>,
    ) -> Result<Vec<Message>, SessionError> {
        context_compaction::refresh_and_materialize_with_policy(
            store,
            session_id,
            fallback_history,
            self.compaction_policy(),
            cancel,
            bus,
        )
        .await
    }

    /// Persist any newly eligible checkpoint ranges after a completed run,
    /// under this engine's policy.
    pub async fn maintain(
        &self,
        store: Arc<dyn SessionStore>,
        session_id: Ulid,
        cancel: CancellationToken,
        bus: Option<&concerto_core::event::EventBus>,
    ) -> Result<(), SessionError> {
        context_compaction::maintain_after_run_with_policy(
            store,
            session_id,
            self.compaction_policy(),
            cancel,
            bus,
        )
        .await
    }

    /// Translate the budget into the compaction policy for the durable path.
    /// The summarization sub-knobs (max excerpt/summary chars, merge width)
    /// are deliberately not config surface in M0 and stay default.
    fn compaction_policy(&self) -> CompactionPolicy {
        CompactionPolicy::from_budget(
            self.budget.trigger_tokens,
            self.budget.retain_user_turns,
            self.budget.minimum_user_turns,
        )
    }
}

impl Default for ContextEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use concerto_core::types::Role;
    use concerto_sessions::SqliteSessionStore;

    fn message(role: Role, content: impl Into<String>) -> Message {
        Message {
            role,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    /// A transcript of `turns` user/assistant pairs of `size`-byte prose.
    fn transcript(turns: usize, fill: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        for turn in 0..turns {
            messages.push(message(Role::User, format!("request {turn} {}", "x".repeat(fill))));
            messages
                .push(message(Role::Assistant, format!("response {turn} {}", "y".repeat(fill))));
        }
        messages
    }

    #[test]
    fn default_budget_is_16000_4_6() {
        let engine = ContextEngine::new();
        assert_eq!(
            engine.budget(),
            ContextBudgetPolicy {
                trigger_tokens: 16_000,
                retain_user_turns: 4,
                minimum_user_turns: 6
            }
        );
        // A `None` section resolves to the same budget (defaults = behavior).
        assert_eq!(ContextEngine::from_config(None).budget(), engine.budget());
    }

    #[test]
    fn budget_resolves_partial_and_full_context_config() {
        let partial = ContextConfig {
            trigger_tokens: Some(8_000),
            retain_user_turns: None,
            minimum_user_turns: None,
        };
        let budget = ContextBudgetPolicy::from_config(Some(&partial));
        assert_eq!(budget.trigger_tokens, 8_000);
        assert_eq!(budget.retain_user_turns, 4, "unset knob keeps default");
        assert_eq!(budget.minimum_user_turns, 6, "unset knob keeps default");

        let full = ContextConfig {
            trigger_tokens: Some(12_000),
            retain_user_turns: Some(2),
            minimum_user_turns: Some(4),
        };
        let budget = ContextBudgetPolicy::from_config(Some(&full));
        assert_eq!(budget.trigger_tokens, 12_000);
        assert_eq!(budget.retain_user_turns, 2);
        assert_eq!(budget.minimum_user_turns, 4);
    }

    #[test]
    fn planner_passes_through_within_budget() {
        let engine = ContextEngine::new();
        // Two tiny messages: (2 bytes / 4 ceil) + 4 per message = 5 + 5 = 10.
        let tiny = vec![message(Role::User, "hi"), message(Role::Assistant, "ok")];
        assert_eq!(engine.plan(&tiny), ContextPlan::PassThrough { estimated_tokens: 10 });

        // Well under the 16000 default trigger.
        let moderate = transcript(4, 100);
        let plan = engine.plan(&moderate);
        assert!(matches!(plan, ContextPlan::PassThrough { .. }));
    }

    #[test]
    fn planner_compacts_when_trigger_exceeded_and_min_turns_met() {
        let engine = ContextEngine::new();
        // 24 turns x 2 x ~1600-byte content: per message ~1604 bytes / 4 =
        // ~405 tokens + 4 overhead => ~19.5k tokens, far past the 16000
        // default trigger, and 24 user turns >= the minimum 6.
        let big = transcript(24, 1600);
        let plan = engine.plan(&big);
        assert!(matches!(plan, ContextPlan::Compact { .. }));
    }

    #[test]
    fn planner_keeps_pass_through_when_user_turns_below_minimum() {
        // A huge single turn, but only 1 user turn < minimum_user_turns (6):
        // compaction must not fire on an early, still-small conversation.
        let engine = ContextEngine::new();
        let messages = vec![message(Role::User, "a".repeat(200_000))];
        // (200_000 bytes / 4 ceil) + 4 = 50_004.
        assert_eq!(engine.plan(&messages), ContextPlan::PassThrough { estimated_tokens: 50_004 });
    }

    #[test]
    fn lower_trigger_via_config_makes_planner_compact() {
        // Same window that passes under defaults must compact when the knob
        // lowers the trigger (ADR-048 §6: config knobs take effect).
        let default = ContextEngine::new();
        let messages = transcript(8, 400);
        assert!(matches!(default.plan(&messages), ContextPlan::PassThrough { .. }));

        let tuned = ContextEngine::from_config(Some(&ContextConfig {
            trigger_tokens: Some(100),
            retain_user_turns: Some(2),
            minimum_user_turns: Some(4),
        }));
        assert!(matches!(tuned.plan(&messages), ContextPlan::Compact { .. }));
    }

    #[tokio::test]
    async fn assemble_bounds_input_tokens_and_preserves_recent_turns() {
        // Simulated long-run: a transcript far past the tuned trigger must
        // assemble to a bounded window that still recalls the most recent
        // turns. The messages are large enough that the deterministic summary
        // (capped internally) provably shrinks the model-input tokens.
        let store = Arc::new(SqliteSessionStore::connect_in_memory().await.unwrap());
        let session = store
            .create_session(
                Utf8Path::new("/tmp/context-engine"),
                "provider",
                "model",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let messages = transcript(15, 2000);
        store.append_messages(session.id, &messages, CancellationToken::new()).await.unwrap();

        let engine = ContextEngine::from_config(Some(&ContextConfig {
            trigger_tokens: Some(1_000),
            retain_user_turns: Some(2),
            minimum_user_turns: Some(4),
        }));
        let active = engine
            .assemble(store.clone(), session.id, &messages, CancellationToken::new(), None)
            .await
            .unwrap();

        // Bounded: the assembled window must be smaller than the full
        // transcript under the production token metric.
        let active_tokens = engine.estimate_tokens(&active);
        let full_tokens = engine.estimate_tokens(&messages);
        assert!(
            active_tokens < full_tokens,
            "bounded assembly must shrink the window: active={active_tokens} full={full_tokens}"
        );

        // Recall: the most recent retained turns remain verbatim.
        assert!(active.iter().any(|entry| entry.content.contains("request 14")));
        assert!(active.iter().any(|entry| entry.content.contains("response 14")));

        // Full transcript still persisted untouched.
        assert_eq!(
            store.load_messages(session.id, CancellationToken::new()).await.unwrap().len(),
            messages.len()
        );

        // Deterministic byte-stable head: a second assemble returns the same
        // active history (no store writes, no date dependence).
        let second = engine
            .assemble(store.clone(), session.id, &messages, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&active).unwrap(),
            serde_json::to_vec(&second).unwrap(),
            "bounded assembly is deterministic"
        );
    }

    #[tokio::test]
    async fn default_engine_assembles_byte_identically_per_turn() {
        // ADR-048 §3: consecutive turns must share a byte-identical rendered
        // head whenever inputs are unchanged. With no compaction firing, the
        // assembled history equals the persisted history exactly.
        let store = Arc::new(SqliteSessionStore::connect_in_memory().await.unwrap());
        let session = store
            .create_session(
                Utf8Path::new("/tmp/context-engine-stable"),
                "provider",
                "model",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let messages = transcript(3, 120);
        store.append_messages(session.id, &messages, CancellationToken::new()).await.unwrap();

        let engine = ContextEngine::new();
        let first = engine
            .assemble(store.clone(), session.id, &messages, CancellationToken::new(), None)
            .await
            .unwrap();
        let second = engine
            .assemble(store.clone(), session.id, &messages, CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap(),
            "rendered head is byte-identical across turns"
        );
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&messages).unwrap(),
            "default budget passes the window through unchanged"
        );
    }
}
