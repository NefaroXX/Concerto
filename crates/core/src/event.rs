//! The event system. Every significant thing the agent does is an `Event`
//! broadcast on the `EventBus`. This is the backbone for the tool activity
//! log, the audit trail, session replay, and observability — all of those
//! are just subscribers, not separate plumbing.

use crate::ids::{new_id, Ulid};
use crate::sanitizer::SecretSanitizer;
use crate::types::{AgentId, McpServerState, TaskId};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};

/// The payload of an event. Variants map directly to the list in the
/// roadmap's "Event system" section. Add new variants here as new phases
/// need them — do not let other crates define their own ad-hoc event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EventKind {
    // --- Phase 0 variants ---
    ToolCalled {
        tool_name: String,
    },
    PolicyVerdict {
        tool_name: String,
        verdict: String,
    },
    MemoryRetrieved {
        project_id: String,
        query_hash: String,
        chunk_count: usize,
        retrieval_ms: u64,
    },
    TokenUsed {
        tokens_in: u64,
        tokens_out: u64,
    },
    CostIncurred {
        cost_usd: f64,
    },
    SessionSaved,
    AgentThought {
        agent_id: String,
        content: String,
    },
    ErrorOccurred {
        message: String,
    },

    // --- tool lifecycle (Phase 2) ---
    ToolExecutionStarted {
        tool_name: String,
        input_hash: String,
        /// Optional human-readable detail, e.g. "write hello.py".
        detail: Option<String>,
    },
    ToolExecutionFinished {
        tool_name: String,
        duration_ms: u64,
        success: bool,
        /// Optional human-readable detail, e.g. "Wrote 42 bytes" or the error.
        detail: Option<String>,
    },
    ToolTimeout {
        tool_name: String,
        timeout_secs: u64,
    },

    // --- policy (Phase 2) ---
    ApprovalRequested {
        tool_name: String,
        timeout_secs: u64,
    },
    ApprovalResolved {
        tool_name: String,
        approved: bool,
    },
    /// An approval request expired before the user responded.
    ///
    /// Emitted by the requester (tool executor) when the approval timeout
    /// elapses; the action is denied by default and never executes.
    ApprovalTimeout {
        tool_name: String,
        timeout_secs: u64,
    },
    PolicyEvaluated {
        tool_name: String,
        verdict: String,
        rule_matched: Option<String>,
    },

    // --- shell streaming (Phase 2) ---
    ShellOutputChunk {
        chunk: String,
        is_stderr: bool,
    },

    // --- spend (Phase 2) ---
    SpendCapApproaching {
        current_usd: f64,
        cap_usd: f64,
        pct: f64,
    },
    SpendCapExceeded {
        session_id: Ulid,
        current_usd: f64,
        cap_usd: f64,
    },
    SpendUpdated {
        session_id: Ulid,
        total_usd: f64,
    },

    // --- agent lifecycle (Phase 3) ---
    /// Assistant text response from an agent, published by the CLI/desktop
    /// so subscribers (UI, session replay, etc.) receive the actual text
    /// rather than reconstructing it from streaming debug events.
    AssistantMessage {
        task_id: TaskId,
        content: String,
    },
    TaskStarted {
        task_id: TaskId,
        description: String,
    },
    TaskCompleted {
        task_id: TaskId,
        success: bool,
    },
    TaskFailed {
        task_id: TaskId,
        error: String,
    },
    AgentStateChanged {
        task_id: TaskId,
        from: String,
        to: String,
    },

    // --- cycle detection (Phase 3) ---
    CycleBudgetExceeded {
        task_id: TaskId,
        tool_name: String,
        call_count: u32,
    },

    // --- context management (Phase 3) ---
    ContextWindowApproaching {
        session_id: Ulid,
        used_tokens: u64,
        capacity_tokens: u64,
    },
    /// Emitted on every compaction decision (ADR-048 §5 auditability).
    ///
    /// `compacted` is true when a new durable checkpoint range was written,
    /// false when the active window passed through unchanged. For durable
    /// compaction the event is published from `maintain_checkpoints` after
    /// the decision, carrying the checkpointed 1-based sequence range when
    /// compaction actually happened.
    ContextCompacted {
        session_id: Ulid,
        active_tokens: u64,
        trigger_tokens: u64,
        compacted: bool,
        /// 1-based message sequence range covered by the new checkpoint,
        /// `(start, end)`; `None` on pass-through and on no-op decisions.
        compacted_range: Option<(u64, u64)>,
    },

    // --- eval (Phase 3) ---
    EvalStarted {
        task_id: TaskId,
        runner: String,
    },
    EvalCompleted {
        task_id: TaskId,
        exit_code: i32,
        passed: bool,
    },

    // --- session undo (Phase 3) ---
    UndoStashCreated {
        session_id: Ulid,
        stash_ref: String,
    },
    UndoRestored {
        session_id: Ulid,
    },

    // --- indexing (Phase 4) ---
    IndexingStarted {
        project_id: String,
        file_count: usize,
    },
    IndexingProgress {
        project_id: String,
        files_processed: usize,
        files_total: usize,
    },
    IndexingCompleted {
        project_id: String,
        chunk_count: usize,
        duration_ms: u64,
    },

    // --- embedding & staleness (Phase 4) ---
    StaleVectorsDetected {
        project_id: String,
        stale_count: usize,
    },
    ReindexQueued {
        project_id: String,
        file_path: String,
        reason: String,
    },
    EmbeddingModelMismatch {
        stored_version: String,
        current_version: String,
    },
    /// The per-project embedder transitioned into a degraded/broken state.
    /// Emitted once per broken-window transition (ADR-39), not per chunk.
    EmbedderDegraded {
        project_id: String,
        reason: String,
    },

    // --- entity extraction (Phase 4) ---
    EntityExtracted {
        project_id: String,
        entity_count: usize,
        relation_count: usize,
    },
    FactExtracted {
        project_id: String,
        fact_count: usize,
    },
    FactExpired {
        project_id: String,
        fact_id: String,
    },

    // --- summarization (Phase 4) ---
    SummarizationStarted {
        session_id: Ulid,
        messages_to_summarize: usize,
    },
    SummarizationCompleted {
        session_id: Ulid,
        summary_len: usize,
    },

    // --- multi-agent lifecycle (Phase 5) ---
    MultiAgentModeStarted {
        task_id: TaskId,
        subtask_count: usize,
        /// Run-scoped id of the planner plan persisted to the plans dir
        /// (`<data>/plans/plan-<id>.json`); `None` when the run carried no
        /// plan file (heuristic fallback, restored run, or persistence was
        /// skipped). Additive field — consumers must not rely on it being
        /// present.
        plan_id: Option<String>,
    },
    MultiAgentModeCompleted {
        task_id: TaskId,
        cost_usd: f64,
    },
    SubTaskCreated {
        task_id: TaskId,
        role: AgentId,
        description: String,
    },
    SubTaskStarted {
        task_id: TaskId,
        role: AgentId,
    },
    SubTaskCompleted {
        task_id: TaskId,
        role: AgentId,
        outcome: String,
    },
    /// A subtask finished but the specialist reported it needs revision
    /// before it can be accepted. Distinct from `SubTaskCompleted` so
    /// consumers never mistake a revision request for completion.
    SubTaskNeedsRevision {
        task_id: TaskId,
        role: AgentId,
        reason: String,
    },
    /// A subtask reported that it is blocked on other tasks.
    SubTaskBlocked {
        task_id: TaskId,
        role: AgentId,
        on: Vec<TaskId>,
    },
    /// A subtask run was cancelled before reaching a terminal outcome.
    SubTaskCancelled {
        task_id: TaskId,
        role: AgentId,
        reason: String,
    },
    SubTaskFailed {
        task_id: TaskId,
        role: AgentId,
        error: String,
    },
    DelegationDecided {
        parent_id: TaskId,
        child_id: TaskId,
        role: AgentId,
        reason: String,
    },

    // --- agent handoff (Phase 2 - relational agent collaboration) ---
    #[serde(rename = "agent_handoff")]
    AgentHandoff {
        from: AgentId,
        to: AgentId,
        task_id: TaskId,
        rationale: String,
    },

    // --- review + validation loops (Phase 5) ---
    ReviewCycleStarted {
        task_id: TaskId,
        cycle_num: u32,
    },
    ReviewCycleCompleted {
        task_id: TaskId,
        cycle_num: u32,
        verdict: String,
    },
    ReviewCycleEscalated {
        task_id: TaskId,
        max_cycles: u32,
    },
    ValidationCycleStarted {
        task_id: TaskId,
        cycle_num: u32,
    },
    ValidationEscalated {
        task_id: TaskId,
        max_cycles: u32,
    },

    // --- routing (Phase 5) ---
    RoutingDecided {
        task_id: TaskId,
        role: AgentId,
        provider: String,
        model: String,
        reason: String,
    },
    BudgetDowngradeTriggered {
        role: AgentId,
        from_model: String,
        to_model: String,
    },

    // --- memory (Phase 5) ---
    MemoryConflict {
        key: String,
        agent_role: AgentId,
        previous_agent: Option<AgentId>,
    },

    // --- cycle detection (Phase 5) ---
    OrchestratorCycleDetected {
        task_id: TaskId,
        sequence: Vec<String>,
    },

    // --- Phase 8: Observability ---
    ObservabilityTraceStarted {
        trace_id: String,
        service_name: String,
    },
    ObservabilityTraceFinished {
        trace_id: String,
        duration_ms: u64,
    },
    ObservabilityMetricExported {
        metric_name: String,
        value: f64,
        labels: Vec<(String, String)>,
    },
    ObservabilityExportFailed {
        exporter: String,
        error: String,
    },

    // --- Phase 8: Eval ---
    EvalBenchmarkStarted {
        suite_name: String,
        task_count: usize,
    },
    EvalBenchmarkCompleted {
        suite_name: String,
        pass_rate: f64,
        avg_latency_ms: u64,
        avg_cost_usd: f64,
    },
    EvalRegressionDetected {
        suite_name: String,
        metric: String,
        delta_pct: f64,
    },

    // --- Phase 8: LSP ---
    LspServerStarted {
        project_dir: String,
        language: String,
    },
    LspServerStopped {
        project_dir: String,
        clean: bool,
    },
    LspServerError {
        project_dir: String,
        error: String,
    },

    // --- ADR-43: MCP server lifecycle ---
    /// An MCP server changed lifecycle state (connecting/connected/failed/
    /// stopped). Published by the `McpManager` watcher for every state
    /// transition so the desktop/CLI can render live per-server health and
    /// crash details without polling. `error` carries the failure detail for
    /// `Failed` transitions and is `None` otherwise.
    McpServerStateChanged {
        server_id: String,
        state: McpServerState,
        error: Option<String>,
    },

    // --- Phase 8: API ---
    OpenAPIDocGenerated {
        path: String,
        endpoint_count: usize,
    },

    // --- Phase 8: Policy ---
    SandboxProfileActivated {
        profile: String,
        tool_name: String,
    },
    SpendCapExceededSession {
        session_id: Ulid,
        current_usd: f64,
        cap_usd: f64,
    },
    SpendCapExceededTask {
        task_id: TaskId,
        current_usd: f64,
        cap_usd: f64,
    },
    SpendCapExceededDaily {
        current_usd: f64,
        cap_usd: f64,
    },
    RateLimitEnforced {
        provider: String,
        rpm: u64,
    },

    // --- Phase 8: Provider cost attribution ---
    ProviderCallCompleted {
        cost: crate::types::CostInfo,
    },

    // --- Phase 8: Auto-update ---
    AutoUpdateAvailable {
        current_version: String,
        latest_version: String,
        download_url: String,
    },

    // --- Phase 10: Provider retry status ---
    /// A provider request is about to be retried after a transient failure.
    ProviderRetryScheduled {
        session_id: Ulid,
        task_id: TaskId,
        attempt: u32,
        delay_ms: u64,
        reason: String,
        source: String,
        /// Raw provider rate-limit signal (e.g. `Retry-After` header),
        /// uncapped, before the local backoff policy clamps it. `None` when
        /// the provider gave no explicit wait time. Kept separate from
        /// `delay_ms` so a "provider says wait 6h" situation is
        /// distinguishable from the clamped local delay.
        retry_after_ms: Option<u64>,
    },

    /// The provider recovered after one or more retries.
    ProviderRetryRecovered {
        session_id: Ulid,
        task_id: TaskId,
        attempts: u32,
        elapsed_ms: u64,
    },

    /// Retries were exhausted (e.g. elapsed-time fuse tripped) without success.
    ProviderRetryExhausted {
        session_id: Ulid,
        task_id: TaskId,
        attempts: u32,
        elapsed_ms: u64,
        reason: String,
        /// Raw provider rate-limit signal from the failing attempt, uncapped
        /// (`None` when the last error carried no explicit wait time, e.g. a
        /// plain 5xx or a local cancellation).
        retry_after_ms: Option<u64>,
    },

    // --- intent routing (ADR-55 Phase 0) ---
    /// A run advanced to a new [`crate::intent::RunStage`].
    ///
    /// ADR-55 Phase 0 ships the type and the event kind only —
    /// `RunStageChanged` is NEVER emitted in Phase 0; it is pure additive
    /// plumbing. Emission lands with the intent-router wiring in a later
    /// phase. Additive variant: consumers must tolerate its absence.
    #[serde(rename = "run_stage_changed")]
    RunStageChanged {
        task_id: TaskId,
        stage: crate::intent::RunStage,
    },
}

impl EventKind {
    /// Sanitize all string fields in this event kind to redact secrets.
    ///
    /// This method creates a new `EventKind` with all string fields passed
    /// through the provided [`SecretSanitizer`]. Non-string fields are
    /// copied as-is.
    ///
    /// # Performance
    ///
    /// This method allocates new strings for each sanitized field. For
    /// high-throughput scenarios, consider sanitizing at the source rather
    /// than post-hoc.
    pub fn sanitized(self, sanitizer: &SecretSanitizer) -> Self {
        match self {
            EventKind::AgentThought { agent_id, content } => EventKind::AgentThought {
                agent_id: sanitizer.sanitize(&agent_id),
                content: sanitizer.sanitize(&content),
            },
            EventKind::ErrorOccurred { message } => {
                EventKind::ErrorOccurred { message: sanitizer.sanitize(&message) }
            }
            EventKind::ShellOutputChunk { chunk, is_stderr } => {
                EventKind::ShellOutputChunk { chunk: sanitizer.sanitize(&chunk), is_stderr }
            }
            EventKind::ToolExecutionFinished { tool_name, duration_ms, success, detail } => {
                EventKind::ToolExecutionFinished {
                    tool_name: sanitizer.sanitize(&tool_name),
                    duration_ms,
                    success,
                    detail: detail.map(|d| sanitizer.sanitize(&d)),
                }
            }
            EventKind::TaskFailed { task_id, error } => {
                EventKind::TaskFailed { task_id, error: sanitizer.sanitize(&error) }
            }
            EventKind::SubTaskCompleted { task_id, role, outcome } => {
                EventKind::SubTaskCompleted { task_id, role, outcome: sanitizer.sanitize(&outcome) }
            }
            EventKind::SubTaskNeedsRevision { task_id, role, reason } => {
                EventKind::SubTaskNeedsRevision {
                    task_id,
                    role,
                    reason: sanitizer.sanitize(&reason),
                }
            }
            EventKind::SubTaskCancelled { task_id, role, reason } => {
                EventKind::SubTaskCancelled { task_id, role, reason: sanitizer.sanitize(&reason) }
            }
            EventKind::SubTaskFailed { task_id, role, error } => {
                EventKind::SubTaskFailed { task_id, role, error: sanitizer.sanitize(&error) }
            }
            EventKind::DelegationDecided { parent_id, child_id, role, reason } => {
                EventKind::DelegationDecided {
                    parent_id,
                    child_id,
                    role,
                    reason: sanitizer.sanitize(&reason),
                }
            }
            EventKind::AgentHandoff { from, to, task_id, rationale } => EventKind::AgentHandoff {
                from,
                to,
                task_id,
                rationale: sanitizer.sanitize(&rationale),
            },
            EventKind::ReviewCycleCompleted { task_id, cycle_num, verdict } => {
                EventKind::ReviewCycleCompleted {
                    task_id,
                    cycle_num,
                    verdict: sanitizer.sanitize(&verdict),
                }
            }
            EventKind::RoutingDecided { task_id, role, provider, model, reason } => {
                EventKind::RoutingDecided {
                    task_id,
                    role,
                    provider: sanitizer.sanitize(&provider),
                    model: sanitizer.sanitize(&model),
                    reason: sanitizer.sanitize(&reason),
                }
            }
            EventKind::ProviderRetryScheduled {
                session_id,
                task_id,
                attempt,
                delay_ms,
                reason,
                source,
                retry_after_ms,
            } => EventKind::ProviderRetryScheduled {
                session_id,
                task_id,
                attempt,
                delay_ms,
                reason: sanitizer.sanitize(&reason),
                source: sanitizer.sanitize(&source),
                retry_after_ms,
            },
            EventKind::ProviderRetryExhausted {
                session_id,
                task_id,
                attempts,
                elapsed_ms,
                reason,
                retry_after_ms,
            } => EventKind::ProviderRetryExhausted {
                session_id,
                task_id,
                attempts,
                elapsed_ms,
                reason: sanitizer.sanitize(&reason),
                retry_after_ms,
            },
            EventKind::ObservabilityExportFailed { exporter, error } => {
                EventKind::ObservabilityExportFailed {
                    exporter: sanitizer.sanitize(&exporter),
                    error: sanitizer.sanitize(&error),
                }
            }
            EventKind::LspServerError { project_dir, error } => EventKind::LspServerError {
                project_dir: sanitizer.sanitize(&project_dir),
                error: sanitizer.sanitize(&error),
            },
            EventKind::McpServerStateChanged { server_id, state, error } => {
                EventKind::McpServerStateChanged {
                    server_id: sanitizer.sanitize(&server_id),
                    state,
                    error: error.map(|e| sanitizer.sanitize(&e)),
                }
            }
            EventKind::EmbedderDegraded { project_id, reason } => EventKind::EmbedderDegraded {
                project_id: sanitizer.sanitize(&project_id),
                reason: sanitizer.sanitize(&reason),
            },
            // All other variants have no string fields or only non-sensitive strings
            other => other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub id: Ulid,
    pub correlation_id: Ulid,
    pub session_id: Ulid,
    pub timestamp: OffsetDateTime,
    pub kind: EventKind,
}

impl Event {
    pub fn new(correlation_id: Ulid, session_id: Ulid, kind: EventKind) -> Self {
        // A `tracing::Span` is created at the call site, not here, so the
        // span covers the work that produced the event, not just its
        // construction. `correlation_id` is set as a span attribute by the
        // caller per the Observability Budget requirement.
        Self {
            id: new_id(),
            correlation_id,
            session_id,
            timestamp: OffsetDateTime::now_utc(),
            kind,
        }
    }
}

/// Broadcast-based event bus.
///
/// Uses `tokio::sync::broadcast` for O(1) fan-out. Slow receivers that
/// fall behind will miss the oldest events — callers should be designed
/// to tolerate this.
///
/// # Secret Sanitization
///
/// When a [`SecretSanitizer`] is configured, all events are sanitized
/// before publication to prevent accidental leakage of API keys, tokens,
/// passwords, and other credentials through the event system.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<Event>>,
    durable_subscribers: Arc<std::sync::Mutex<Vec<Arc<DurableSender>>>>,
    /// Unique identifier for this bus instance, used for `Hash`/`Eq` in
    /// iced 0.14 `Subscription::run_with`.
    id: u64,
    /// Optional secret sanitizer for redacting sensitive information.
    sanitizer: Option<Arc<SecretSanitizer>>,
}

static BUS_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Latches the one-time `subscribe_durable` subscriber-cap warning per process.
static DURABLE_SUBSCRIBER_CAP_WARNED: AtomicBool = AtomicBool::new(false);

impl std::hash::Hash for EventBus {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialEq for EventBus {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for EventBus {}

/// Receiver half of `EventBus`. Wraps `broadcast::Receiver`.
#[derive(Debug)]
pub struct EventReceiver {
    rx: broadcast::Receiver<Arc<Event>>,
}

/// Durable-subscription backlog thresholds (long-session robustness observability).
///
/// These bound how far a slow or leaked durable consumer may fall behind before
/// it is flagged. Delivery stays lossless: the bus only observes and warns, it
/// never drops a durable event.
const DURABLE_PENDING_WARN: usize = 4096;
const DURABLE_PENDING_LAG: usize = 65536;
const MAX_DURABLE_SUBSCRIBERS: usize = 32;

/// Wrapper around one durable subscriber's unbounded channel plus the shared,
/// atomic observability state that the publisher and the receiver keep in sync.
///
/// Stored behind an `Arc` in [`EventBus`] so the publisher (via the vector) and
/// the receiver (via its own `Arc` handles) observe the same counters.
struct DurableSender {
    tx: mpsc::UnboundedSender<Arc<Event>>,
    /// Approximate number of events buffered for this subscriber.
    pending: Arc<AtomicUsize>,
    /// Rate-limiter for the warn log; reset when the backlog drains below half.
    warned: Arc<AtomicBool>,
    /// Latches once the subscriber exceeds `DURABLE_PENDING_LAG`.
    lagging: Arc<AtomicBool>,
}

/// Read-only snapshot of durable-subscriber health for tests and ops tooling.
#[derive(Debug, Clone, Copy)]
pub struct DurableHealth {
    pub subscriber_count: usize,
    pub max_pending: usize,
    pub lagging_subscribers: usize,
}

/// Remove durable subscribers whose channel has closed (receiver dropped) so a
/// list of dead receivers never grows without bound.
fn prune_durable(subscribers: &mut Vec<Arc<DurableSender>>) {
    subscribers.retain(|sender| !sender.tx.is_closed());
}

/// Lossless per-process receiver for persistence and replay materialization.
///
/// Unlike the broadcast receiver, this queue does not discard older events
/// when the consumer temporarily falls behind. It is intended for one durable
/// recorder per active session, not UI fan-out.
#[derive(Debug)]
pub struct DurableEventReceiver {
    rx: mpsc::UnboundedReceiver<Arc<Event>>,
    pending: Arc<AtomicUsize>,
    warned: Arc<AtomicBool>,
    lagging: Arc<AtomicBool>,
}

impl DurableEventReceiver {
    /// Decrement the shared pending counter after an event is consumed, and
    /// re-arm the warn mitigation once the backlog drains below half the warn
    /// threshold.
    fn on_consumed(&self) {
        let _ =
            self.pending.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| p.checked_sub(1));
        if self.pending.load(Ordering::Relaxed) < DURABLE_PENDING_WARN / 2 {
            self.warned.store(false, Ordering::Relaxed);
        }
        if self.pending.load(Ordering::Relaxed) < DURABLE_PENDING_LAG {
            self.lagging.store(false, Ordering::Relaxed);
        }
    }

    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        match self.rx.recv().await {
            Some(event) => {
                self.on_consumed();
                Some(event)
            }
            None => None,
        }
    }

    pub fn try_recv(&mut self) -> Result<Arc<Event>, mpsc::error::TryRecvError> {
        match self.rx.try_recv() {
            Ok(event) => {
                self.on_consumed();
                Ok(event)
            }
            Err(error) => Err(error),
        }
    }
}

impl EventReceiver {
    /// Receive the next event, awaiting until one is available.
    /// Returns `Err(RecvError::Closed)` when all sender handles have been
    /// dropped. Returns `Err(RecvError::Lagged(n))` when the receiver
    /// fell behind and `n` events were skipped.
    pub async fn recv(&mut self) -> Result<Arc<Event>, RecvError> {
        self.rx.recv().await
    }

    /// Receive an already-buffered event without waiting.
    pub fn try_recv(&mut self) -> Result<Arc<Event>, broadcast::error::TryRecvError> {
        self.rx.try_recv()
    }

    /// Consume the receiver, returning the inner `broadcast::Receiver`
    /// for use with `tokio_stream::wrappers::BroadcastStream` or other
    /// adapters.
    pub fn into_inner(self) -> broadcast::Receiver<Arc<Event>> {
        self.rx
    }
}

const DEFAULT_CHANNEL_CAPACITY: usize = 256;

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let cap = if capacity == 0 { DEFAULT_CHANNEL_CAPACITY } else { capacity };
        let (tx, _) = broadcast::channel(cap);
        let id = BUS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            tx,
            durable_subscribers: Arc::new(std::sync::Mutex::new(Vec::new())),
            id,
            sanitizer: None,
        }
    }

    /// Create an event bus with secret sanitization enabled.
    ///
    /// All events published to this bus will have their string fields
    /// sanitized before broadcast to prevent credential leakage.
    pub fn with_sanitizer(capacity: usize, sanitizer: SecretSanitizer) -> Self {
        let cap = if capacity == 0 { DEFAULT_CHANNEL_CAPACITY } else { capacity };
        let (tx, _) = broadcast::channel(cap);
        let id = BUS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            tx,
            durable_subscribers: Arc::new(std::sync::Mutex::new(Vec::new())),
            id,
            sanitizer: Some(Arc::new(sanitizer)),
        }
    }

    pub fn subscribe(&self) -> EventReceiver {
        EventReceiver { rx: self.tx.subscribe() }
    }

    /// Subscribe a lossless per-process persistence consumer.
    pub fn subscribe_durable(&self) -> DurableEventReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let warned = Arc::new(AtomicBool::new(false));
        let lagging = Arc::new(AtomicBool::new(false));
        let sender = Arc::new(DurableSender {
            tx,
            pending: Arc::clone(&pending),
            warned: Arc::clone(&warned),
            lagging: Arc::clone(&lagging),
        });
        let mut subscribers =
            self.durable_subscribers.lock().unwrap_or_else(|error| error.into_inner());
        prune_durable(&mut subscribers);
        if subscribers.len() > MAX_DURABLE_SUBSCRIBERS
            && !DURABLE_SUBSCRIBER_CAP_WARNED.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                count = subscribers.len(),
                max = MAX_DURABLE_SUBSCRIBERS,
                "durable subscriber count exceeds configured cap; possible runaway accumulation"
            );
        }
        subscribers.push(sender);
        DurableEventReceiver { rx, pending, warned, lagging }
    }

    /// Read-only durable-subscriber health snapshot (tests + ops).
    pub fn durable_health(&self) -> DurableHealth {
        let subscribers =
            self.durable_subscribers.lock().unwrap_or_else(|error| error.into_inner());
        let mut max_pending = 0;
        let mut lagging_subscribers = 0;
        for sender in subscribers.iter() {
            let pending = sender.pending.load(Ordering::Relaxed);
            if pending > max_pending {
                max_pending = pending;
            }
            if sender.lagging.load(Ordering::Relaxed) {
                lagging_subscribers += 1;
            }
        }
        DurableHealth { subscriber_count: subscribers.len(), max_pending, lagging_subscribers }
    }

    /// Publish an event to all active subscribers.
    /// If no receivers are active, emits a `tracing::debug!` instead of
    /// returning an error.
    ///
    /// If a sanitizer is configured, the event's string fields are
    /// sanitized before publication.
    pub fn publish(&self, event: Event) -> Result<(), crate::error::CoreError> {
        let event = if let Some(ref sanitizer) = self.sanitizer {
            Event {
                id: event.id,
                correlation_id: event.correlation_id,
                session_id: event.session_id,
                timestamp: event.timestamp,
                kind: event.kind.sanitized(sanitizer),
            }
        } else {
            event
        };
        let event = Arc::new(event);
        {
            let mut subscribers =
                self.durable_subscribers.lock().unwrap_or_else(|error| error.into_inner());
            prune_durable(&mut subscribers);
            for sender in subscribers.iter() {
                if sender.tx.send(Arc::clone(&event)).is_err() {
                    // Channel closed (receiver dropped); pruned on a later pass.
                    continue;
                }
                let pending = sender.pending.fetch_add(1, Ordering::Relaxed) + 1;
                if pending > DURABLE_PENDING_LAG {
                    sender.lagging.store(true, Ordering::Relaxed);
                    sender.warned.store(true, Ordering::Relaxed);
                } else if pending >= DURABLE_PENDING_WARN
                    && !sender.warned.swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(
                        pending,
                        warn_threshold = DURABLE_PENDING_WARN,
                        "durable subscriber backlog exceeded warn threshold; consumer may be stalled"
                    );
                }
            }
        }
        match self.tx.send(event) {
            Ok(_) => {}
            Err(broadcast::error::SendError(_)) => {
                tracing::debug!("EventBus::publish: no active subscribers");
            }
        }
        Ok(())
    }

    /// Publish an `EventKind` directly, wrapping it in a minimal Event with
    /// newly generated IDs and the current timestamp.
    pub fn publish_raw(&self, kind: EventKind) -> Result<(), crate::error::CoreError> {
        let event = Event::new(new_id(), new_id(), kind);
        self.publish(event)
    }

    /// Publish an event bound to a specific session and correlation id.
    ///
    /// Use this for agent/task/session-scoped events so replay and audit
    /// subscribers can correlate them to the right persistent session.
    pub fn publish_for_session(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        kind: EventKind,
    ) -> Result<(), crate::error::CoreError> {
        let event = Event::new(correlation_id, session_id, kind);
        self.publish(event)
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CHANNEL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 0 exit criterion: "emit 10 events with correlation IDs,
    /// subscribe and print them with timestamps." This test is the
    /// machine-checked version of that demo; `examples/event_loop_demo.rs`
    /// is the human-readable one for the terminal.
    #[tokio::test]
    async fn ten_events_round_trip_with_correlation_id() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        let correlation_id = new_id();
        let session_id = new_id();

        for i in 0..10 {
            let event = Event::new(
                correlation_id,
                session_id,
                EventKind::AgentThought {
                    agent_id: "demo".into(),
                    content: format!("thought #{i}"),
                },
            );
            bus.publish(event).expect("at least one subscriber exists");
        }

        let mut received = 0;
        while received < 10 {
            let event = rx.recv().await.expect("channel should not close");
            assert_eq!(event.correlation_id, correlation_id);
            received += 1;
        }
        assert_eq!(received, 10);
    }

    #[tokio::test]
    async fn durable_receiver_does_not_drop_burst_events() {
        let bus = EventBus::new(4);
        let mut durable = bus.subscribe_durable();
        let session_id = new_id();
        let correlation_id = new_id();

        for i in 0..1_000 {
            bus.publish_for_session(
                session_id,
                correlation_id,
                EventKind::AgentThought { agent_id: "burst".into(), content: i.to_string() },
            )
            .unwrap();
        }

        for expected in 0..1_000 {
            let event = durable.recv().await.unwrap();
            let EventKind::AgentThought { content, .. } = &event.kind else {
                panic!("unexpected event kind");
            };
            assert_eq!(content, &expected.to_string());
        }
    }

    /// Long-session robustness: durable delivery is lossless. Publishing 1000
    /// events to one durable subscriber and draining them yields exactly 1000
    /// in order, and the backlog fully empties (health stays bounded).
    #[test]
    fn durable_delivery_is_lossless() {
        let bus = EventBus::default();
        let mut durable = bus.subscribe_durable();
        let session_id = new_id();
        let correlation_id = new_id();

        for i in 0..1_000 {
            bus.publish_for_session(
                session_id,
                correlation_id,
                EventKind::AgentThought { agent_id: "lossless".into(), content: i.to_string() },
            )
            .unwrap();
        }

        let mut received = 0;
        while let Ok(event) = durable.try_recv() {
            let EventKind::AgentThought { content, .. } = &event.kind else {
                panic!("unexpected event kind");
            };
            assert_eq!(content, &received.to_string());
            received += 1;
        }
        assert_eq!(received, 1_000);
        assert_eq!(bus.durable_health().max_pending, 0);
    }

    /// The observability counter tracks how many events are actually buffered
    /// for a subscriber: rises with publishes, drops back to zero once drained.
    #[test]
    fn durable_pending_tracks_buffered_events() {
        let bus = EventBus::default();
        let mut durable = bus.subscribe_durable();
        let session_id = new_id();
        let correlation_id = new_id();
        let n = 200;

        for i in 0..n {
            bus.publish_for_session(
                session_id,
                correlation_id,
                EventKind::AgentThought { agent_id: "track".into(), content: i.to_string() },
            )
            .unwrap();
        }
        assert_eq!(bus.durable_health().max_pending, n);

        let mut received = 0;
        while durable.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, n);
        assert_eq!(bus.durable_health().max_pending, 0);
    }

    /// Dead (dropped) receivers are pruned from the subscriber list both on
    /// publish and on subscribe, so they never grow the vector without bound.
    #[test]
    fn durable_prunes_closed_subscribers_on_publish_and_subscribe() {
        let bus = EventBus::default();
        let mut a = bus.subscribe_durable();
        let b = bus.subscribe_durable();
        let c = bus.subscribe_durable();

        // Publish-side retain: drop a receiver, publish, it is pruned.
        drop(b);
        bus.publish(Event::new(new_id(), new_id(), EventKind::SessionSaved)).unwrap();
        assert_eq!(bus.durable_health().subscriber_count, 2);

        // Subscribe-side prune: drop another receiver, a subscribe re-prunes.
        drop(c);
        bus.subscribe_durable();
        assert_eq!(bus.durable_health().subscriber_count, 2);

        // The surviving receiver still gets its event.
        assert!(a.try_recv().is_ok());
    }

    /// A durable subscriber that never drains past the lag threshold is flagged
    /// in the read-only health accessor (no timing/sleeps — deterministic).
    #[test]
    fn durable_lag_health_flags_when_backlogged() {
        let bus = EventBus::default();
        let _never_drains = bus.subscribe_durable();
        let session_id = new_id();
        let correlation_id = new_id();

        for i in 0..(DURABLE_PENDING_LAG + 1) {
            bus.publish_for_session(
                session_id,
                correlation_id,
                EventKind::AgentThought { agent_id: "lag".into(), content: i.to_string() },
            )
            .unwrap();
        }

        let health = bus.durable_health();
        assert!(health.lagging_subscribers >= 1);
        assert_eq!(health.max_pending, DURABLE_PENDING_LAG + 1);
    }

    #[tokio::test]
    async fn sanitizer_redacts_secrets_in_events() {
        let sanitizer = SecretSanitizer::default();
        let bus = EventBus::with_sanitizer(16, sanitizer);
        let mut rx = bus.subscribe();

        let event = Event::new(
            new_id(),
            new_id(),
            EventKind::AgentThought {
                agent_id: "test".into(),
                content: "Using OpenAI key sk-1234567890abcdef1234567890abcdef".into(),
            },
        );
        bus.publish(event).unwrap();

        let received = rx.recv().await.unwrap();
        if let EventKind::AgentThought { content, .. } = &received.kind {
            assert!(content.contains("[REDACTED]"));
            assert!(!content.contains("sk-1234567890abcdef"));
        } else {
            panic!("Expected AgentThought event");
        }
    }

    #[tokio::test]
    async fn sanitizer_redacts_bearer_tokens() {
        let sanitizer = SecretSanitizer::default();
        let bus = EventBus::with_sanitizer(16, sanitizer);
        let mut rx = bus.subscribe();

        let event = Event::new(
            new_id(),
            new_id(),
            EventKind::ErrorOccurred {
                message: "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9".into(),
            },
        );
        bus.publish(event).unwrap();

        let received = rx.recv().await.unwrap();
        if let EventKind::ErrorOccurred { message } = &received.kind {
            assert!(message.contains("[REDACTED]"));
            assert!(!message.contains("eyJhbGci"));
        } else {
            panic!("Expected ErrorOccurred event");
        }
    }

    #[tokio::test]
    async fn sanitizer_redacts_passwords() {
        let sanitizer = SecretSanitizer::default();
        let bus = EventBus::with_sanitizer(16, sanitizer);
        let mut rx = bus.subscribe();

        let event = Event::new(
            new_id(),
            new_id(),
            EventKind::ShellOutputChunk {
                chunk: "password=mysecretpassword123".into(),
                is_stderr: false,
            },
        );
        bus.publish(event).unwrap();

        let received = rx.recv().await.unwrap();
        if let EventKind::ShellOutputChunk { chunk, .. } = &received.kind {
            assert!(chunk.contains("[REDACTED]"));
            assert!(!chunk.contains("mysecretpassword123"));
        } else {
            panic!("Expected ShellOutputChunk event");
        }
    }

    #[test]
    fn event_bus_without_sanitizer_preserves_content() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();

        let original_content = "sk-1234567890abcdef1234567890abcdef";
        let event = Event::new(
            new_id(),
            new_id(),
            EventKind::AgentThought { agent_id: "test".into(), content: original_content.into() },
        );
        bus.publish(event).unwrap();

        let received = rx.try_recv().unwrap();
        if let EventKind::AgentThought { content, .. } = &received.kind {
            assert_eq!(content, original_content);
        } else {
            panic!("Expected AgentThought event");
        }
    }

    #[test]
    fn event_has_correlation_id() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let cid = new_id();
        let sid = new_id();
        let event = Event::new(cid, sid, EventKind::SessionSaved);
        bus.publish(event).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.correlation_id, cid);
        assert_eq!(received.session_id, sid);
    }

    #[test]
    fn event_timestamp_is_set() {
        let event = Event::new(new_id(), new_id(), EventKind::SessionSaved);
        // Verify timestamp is within reasonable range (not zero/unset)
        let ts = event.timestamp.unix_timestamp_nanos();
        assert!(ts > 0, "timestamp should not be zero");
    }

    #[test]
    fn event_kind_variant_count() {
        // Ensure all expected variants at least exist and can be constructed
        let kinds: Vec<EventKind> = vec![
            EventKind::ToolCalled { tool_name: "test".into() },
            EventKind::PolicyVerdict { tool_name: "test".into(), verdict: "allow".into() },
            EventKind::MemoryRetrieved {
                project_id: "p".into(),
                query_hash: "h".into(),
                chunk_count: 5,
                retrieval_ms: 100,
            },
            EventKind::TokenUsed { tokens_in: 100, tokens_out: 50 },
            EventKind::CostIncurred { cost_usd: 0.01 },
            EventKind::SessionSaved,
            EventKind::AgentThought { agent_id: "a1".into(), content: "thinking".into() },
            EventKind::ErrorOccurred { message: "error".into() },
        ];
        assert_eq!(kinds.len(), 8);
    }

    #[tokio::test]
    async fn event_bus_subscribe_after_publish_misses_events() {
        let bus = EventBus::default();
        let event = Event::new(new_id(), new_id(), EventKind::SessionSaved);
        bus.publish(event).unwrap();
        // Subscribe after publish — should not receive the already-published event
        let mut rx = bus.subscribe();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_err(), "late subscriber should not receive past events");
    }

    #[tokio::test]
    async fn event_bus_multiple_subscribers_all_receive() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(Event::new(new_id(), new_id(), EventKind::SessionSaved)).unwrap();

        let r1 = tokio::time::timeout(std::time::Duration::from_secs(1), rx1.recv()).await;
        let r2 = tokio::time::timeout(std::time::Duration::from_secs(1), rx2.recv()).await;
        assert!(r1.is_ok(), "first subscriber should receive");
        assert!(r2.is_ok(), "second subscriber should receive");
    }

    #[test]
    fn event_bus_send_capacity_default() {
        let bus = EventBus::default();
        // Default capacity should be at least 16
        let event = Event::new(new_id(), new_id(), EventKind::SessionSaved);
        for _ in 0..16 {
            bus.publish(event.clone()).unwrap();
        }
    }

    #[test]
    fn event_kind_serialization_roundtrip() {
        let kind = EventKind::ToolCalled { tool_name: "shell".into() };
        let json = serde_json::to_value(&kind).unwrap();
        let back: EventKind = serde_json::from_value(json).unwrap();
        match back {
            EventKind::ToolCalled { tool_name } => assert_eq!(tool_name, "shell"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// The run-stage transition event must survive serde round-trip with its
    /// payload intact (ADR-55 Phase 2a: emitted from the run wrapper, consumed
    /// by the desktop/cli bus adapters).
    #[test]
    fn run_stage_changed_serialization_roundtrip() {
        let task_id = crate::types::TaskId::new();
        let kind = EventKind::RunStageChanged { task_id, stage: crate::intent::RunStage::Execute };
        let json = serde_json::to_value(&kind).unwrap();
        let back: EventKind = serde_json::from_value(json).unwrap();
        match back {
            EventKind::RunStageChanged { task_id: back_task_id, stage } => {
                assert_eq!(back_task_id, task_id);
                assert_eq!(stage, crate::intent::RunStage::Execute);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn event_clone_preserves_all_fields() {
        let original = Event::new(new_id(), new_id(), EventKind::SessionSaved);
        let cloned = original.clone();
        assert_eq!(original.id, cloned.id);
        assert_eq!(original.correlation_id, cloned.correlation_id);
        assert_eq!(original.session_id, cloned.session_id);
        assert_eq!(original.timestamp, cloned.timestamp);
    }

    /// Verify that `publish_for_session` publishes events correctly and
    /// subscribers receive them with the right session correlation.
    #[tokio::test]
    async fn publish_for_session_routes_to_subscribers() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let cid = new_id();
        let sid = new_id();

        let _ = bus.publish_for_session(sid, cid, EventKind::SessionSaved);

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive event")
            .expect("event should not be an error");
        assert_eq!(received.correlation_id, cid);
        assert_eq!(received.session_id, sid);
    }

    /// Verify that `EventKind` variants implement `Debug` without panicking
    /// for all common variants.
    #[test]
    fn event_kind_debug_output() {
        let variants: Vec<EventKind> = vec![
            EventKind::SessionSaved,
            EventKind::TaskStarted { task_id: TaskId::new(), description: "test".into() },
            EventKind::ToolCalled { tool_name: "echo".into() },
            EventKind::MemoryRetrieved {
                project_id: "p".into(),
                query_hash: "h".into(),
                chunk_count: 3,
                retrieval_ms: 50,
            },
            EventKind::AgentThought { agent_id: "agent".into(), content: "think".into() },
        ];
        for v in &variants {
            let debug = format!("{v:?}");
            assert!(!debug.is_empty(), "Debug output must not be empty for {v:?}");
        }
    }

    /// The ADR-43 MCP state event must survive a serialization round trip so
    /// the desktop/CLI can persist and replay server health transitions.
    #[test]
    fn mcp_server_state_changed_serializes_round_trip() {
        let kind = EventKind::McpServerStateChanged {
            server_id: "github".into(),
            state: crate::types::McpServerState::Failed,
            error: Some("server exited with status 1".into()),
        };
        let json = serde_json::to_string(&kind).expect("event must serialize");
        let back: EventKind = serde_json::from_str(&json).expect("event must deserialize");
        assert!(matches!(
            back,
            EventKind::McpServerStateChanged { server_id, state, error }
                if server_id == "github"
                    && state == crate::types::McpServerState::Failed
                    && error.as_deref() == Some("server exited with status 1")
        ));
        // The state enum itself serializes too (used by the manager accessors).
        let state_json = serde_json::to_string(&crate::types::McpServerState::Connected)
            .expect("must serialize");
        let state_back: crate::types::McpServerState =
            serde_json::from_str(&state_json).expect("must deserialize");
        assert_eq!(state_back, crate::types::McpServerState::Connected);
    }
}
