//! Run recording & audit-sink plumbing for `runtime_runner_impl`.
//!
//! This module owns the durable recording side of a run: the event recorder
//! (session event persistence), the transcript recorder (ADR-36 stage 2
//! correlation/merge plus ADR-58 P2+P3 gate labels), and the no-op audit sink
//! used when no UI logging is configured. Nothing here owns run control flow:
//! the parent's `create_session_and_recorder` constructs these guards and the
//! parent stops them at the run boundaries, so the guard API and the exact
//! merge/flush/settle semantics move verbatim.
//!
//! Stability contract: transcript ordering (a `Running` tool call holds its
//! position until its terminal event merges in place), the batch-flush
//! threshold, and the ADR-36 settle-on-stop behavior are observable and pinned
//! by the dedicated recorder tests in `runtime_runner/tests/mod.rs` (NORM S15);
//! those tests keep running through the parent's `pub(crate) use recorders::*;`
//! re-export, so no coverage moves or is deleted.

#[path = "recorders/health.rs"]
mod health;

use concerto_core::recording::{RecorderKind, RecorderOutcome};
use health::{drain_task, RecorderHealth, WRITE_TIMEOUT};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use concerto_config::{BlueprintFacade, ResolvedBlueprint, StageKind};
use concerto_core::error::PolicyError;
use concerto_core::event::{DurableEventReceiver, Event, EventBus};
use concerto_core::ids::Ulid;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::transcript::{
    transcript_entry_from_event_with_labels, GateLabels, TranscriptEntry, TranscriptToolStatus,
};
use concerto_core::CancellationToken;

/// Simple no‑op audit logger used when no UI logging is required.
pub(crate) struct NoopAuditLog;

/// Rate-limited consumer-side surfacing of durable queue overflow.
///
/// The same gap is retained in terminal recorder health. The cumulative drop
/// count is compared on every receive
/// step; an error is emitted at most once per observed gap, and only at the
/// first gap or each doubling of the cumulative count, so a persistently
/// lagging queue cannot flood the log.
struct GapLog {
    consumer: &'static str,
    health: Arc<RecorderHealth>,
    observed: u64,
    logged: u64,
}

impl GapLog {
    fn new(consumer: &'static str, health: Arc<RecorderHealth>) -> Self {
        Self { consumer, health, observed: 0, logged: 0 }
    }

    fn observe(&mut self, receiver: &DurableEventReceiver, session_id: Ulid) {
        let current = receiver.dropped_events();
        if current <= self.observed {
            return;
        }
        let gap = current - self.observed;
        self.observed = current;
        self.health.dropped(current);
        if self.logged == 0 || current >= self.logged.saturating_mul(2) {
            self.logged = current;
            tracing::error!(
                consumer = self.consumer,
                %session_id,
                dropped = current,
                gap,
                capacity = receiver.capacity(),
                "durable event consumer missed events: queue overflowed"
            );
        }
    }
}

pub(crate) struct EventRecorderGuard {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    health: Arc<RecorderHealth>,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    bus: EventBus,
}

impl EventRecorderGuard {
    pub(crate) async fn stop(mut self) -> RecorderOutcome {
        drain_task(&mut self.task, &self.health, &self.cancel).await;
        self.health.persist(&self.store, self.session_id, &self.bus, CancellationToken::new()).await
    }
}

impl Drop for EventRecorderGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn persist_event(
    store: &Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    event: &Event,
    health: &RecorderHealth,
    cancel: &CancellationToken,
) {
    if event.session_id != session_id {
        return;
    }
    let write_cancel = cancel.child_token();
    let _cancel_on_drop = write_cancel.clone().drop_guard();
    match tokio::time::timeout(WRITE_TIMEOUT, store.record_event(session_id, event, write_cancel))
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            health.failed_write();
            tracing::warn!(%error, %session_id, "failed to persist session event");
        }
        Err(_) => {
            health.failed_write();
            tracing::warn!(%session_id, "session event write timed out");
        }
    }
}

pub(crate) fn start_event_recorder(
    bus: &EventBus,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
) -> EventRecorderGuard {
    let mut receiver = bus.subscribe_durable_for_session(session_id);
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let flush_cancel = CancellationToken::new();
    let health = Arc::new(RecorderHealth::new(RecorderKind::EventLog));
    let task_health = health.clone();
    let task_store = store.clone();
    let task = tokio::spawn(async move {
        let mut gap = GapLog::new("session-event-recorder", task_health.clone());
        loop {
            let event = tokio::select! {
                _ = task_cancel.cancelled() => break,
                event = receiver.recv() => event,
            };
            gap.observe(&receiver, session_id);
            let Some(event) = event else { break };
            persist_event(&task_store, session_id, &event, &task_health, &flush_cancel).await;
        }
        // Close before draining: late publishers cannot extend the queue or
        // change its drop counter after the final health observation.
        receiver.close();
        while let Ok(event) = receiver.try_recv() {
            gap.observe(&receiver, session_id);
            persist_event(&task_store, session_id, &event, &task_health, &flush_cancel).await;
        }
        gap.observe(&receiver, session_id);
    });
    EventRecorderGuard { cancel, task: Some(task), health, store, session_id, bus: bus.clone() }
}

/// Flush a batch once this many transcript-relevant events have accumulated in
/// the recorder's buffer. Entries are written in event order; anything still
/// buffered when the run ends is flushed by [`TranscriptRecorderGuard::stop`].
const TRANSCRIPT_FLUSH_THRESHOLD: usize = 32;

/// Correlation buffer shared between the transcript recorder task and its
/// guard methods. Entries are kept in event order until flushed to the store.
///
/// A `Running` tool call is deliberately kept buffered even across batches:
/// the store is append-only, so the terminal/approval event must merge into it
/// in place (ADR-36 §2) before anything behind it is written.
struct TranscriptRecorderState {
    pending: Vec<TranscriptEntry>,
}

impl TranscriptRecorderState {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Correlate one mapped event entry into the in-order buffer. A `Running`
    /// tool call stays buffered; a terminal tool event merges into the last
    /// `Running` entry with the same tool name (updating its status and
    /// appending the terminal detail), so exactly one entry per invocation is
    /// persisted.
    fn merge(&mut self, entry: TranscriptEntry) {
        match entry {
            TranscriptEntry::ToolCall { status: TranscriptToolStatus::Running, .. } => {
                self.pending.push(entry);
            }
            TranscriptEntry::ToolCall { tool_name, status, detail } => {
                let merge_index = self.pending.iter().rposition(|existing| {
                    matches!(existing,
                        TranscriptEntry::ToolCall { tool_name: n, status: TranscriptToolStatus::Running, .. } if *n == tool_name)
                });
                match merge_index {
                    Some(index) => {
                        if let TranscriptEntry::ToolCall {
                            status: existing_status,
                            detail: existing_detail,
                            ..
                        } = &mut self.pending[index]
                        {
                            *existing_status = status;
                            if !detail.is_empty() {
                                if existing_detail.is_empty() {
                                    *existing_detail = detail;
                                } else {
                                    existing_detail.push('\n');
                                    existing_detail.push_str(&detail);
                                }
                            }
                        }
                    }
                    // The start event was missed (e.g. it fired before this
                    // recorder subscribed or for another session): persist the
                    // terminal outcome as its own entry, defensively.
                    None => {
                        self.pending.push(TranscriptEntry::ToolCall { tool_name, status, detail })
                    }
                }
            }
            other => self.pending.push(other),
        }
    }

    /// Extract the longest in-order prefix of settled entries (everything
    /// before the first still-`Running` tool call), preserving event order in
    /// the store.
    fn drain_settled_prefix(&mut self) -> Vec<TranscriptEntry> {
        let flushable = self
            .pending
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::ToolCall { status: TranscriptToolStatus::Running, .. }
                )
            })
            .unwrap_or(self.pending.len());
        self.pending.drain(..flushable).collect()
    }

    /// When enough entries have accumulated, flush the settled prefix. Returns
    /// an empty vec when the batch is not yet large enough or a `Running`
    /// entry still holds the front of the buffer.
    fn drain_batch(&mut self) -> Vec<TranscriptEntry> {
        if self.pending.len() < TRANSCRIPT_FLUSH_THRESHOLD {
            return Vec::new();
        }
        self.drain_settled_prefix()
    }

    /// ADR-36 settle-on-stop: any tool call still `Running` at run end is
    /// recorded as `Cancelled` (mirrors the desktop `settle_running_tool_calls`).
    fn settle_running(&mut self) {
        for entry in self.pending.iter_mut() {
            if let TranscriptEntry::ToolCall { status, .. } = entry {
                if *status == TranscriptToolStatus::Running {
                    *status = TranscriptToolStatus::Cancelled;
                }
            }
        }
    }
}

/// Persist a batch of transcript entries, best-effort: on failure log a
/// warning and drop the batch, keeping the recorder running (matches the event
/// recorder's `record_event` resilience).
async fn flush_entries(
    store: &Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    entries: Vec<TranscriptEntry>,
    cancel: &CancellationToken,
    health: &RecorderHealth,
) {
    if entries.is_empty() {
        return;
    }
    let write_cancel = cancel.child_token();
    let _cancel_on_drop = write_cancel.clone().drop_guard();
    match tokio::time::timeout(
        WRITE_TIMEOUT,
        store.append_transcript(session_id, &entries, write_cancel),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            health.failed_write();
            tracing::warn!(%error, %session_id, "failed to flush transcript entries");
        }
        Err(_) => {
            health.failed_write();
            tracing::warn!(%session_id, "transcript write timed out");
        }
    }
}

/// Map one bus event to a transcript entry and merge it into the recorder's
/// in-order buffer, flushing a batch once the buffer is large enough.
///
/// ADR-58 P2+P3 (F8): gate labels come from the resolved blueprint (threaded
/// through the recorder), so a renamed review/validate stage renders its
/// configured label; the defaults keep the standard blueprint byte-identical.
async fn handle_transcript_event(
    event: &Event,
    state: &Arc<Mutex<TranscriptRecorderState>>,
    store: &Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    flush_cancel: &CancellationToken,
    gate_labels: &GateLabels,
    health: &RecorderHealth,
) {
    let Some(entry) = transcript_entry_from_event_with_labels(&event.kind, gate_labels) else {
        return;
    };
    let batch = {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.merge(entry);
        state.drain_batch()
    };
    flush_entries(store, session_id, batch, flush_cancel, health).await;
}

/// Filter events from other sessions and forward this session's events to the
/// transcript recorder (mirrors the event recorder's skip style).
async fn handle_bus_event(
    event: Arc<Event>,
    session_id: Ulid,
    state: &Arc<Mutex<TranscriptRecorderState>>,
    store: &Arc<dyn concerto_sessions::SessionStore>,
    flush_cancel: &CancellationToken,
    gate_labels: &GateLabels,
    health: &RecorderHealth,
) {
    if event.session_id != session_id {
        tracing::debug!(
            recorder_session = %session_id,
            event_session = %event.session_id,
            kind = ?event.kind,
            "skipping transcript event for different session",
        );
        return;
    }
    handle_transcript_event(&event, state, store, session_id, flush_cancel, gate_labels, health)
        .await;
}

pub(crate) struct TranscriptRecorderGuard {
    cancel: CancellationToken,
    /// Never cancelled while the guard lives; used for every `append_transcript`
    /// call so the final flush succeeds even when the run (or this recorder)
    /// was cancelled.
    flush_cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    state: Arc<Mutex<TranscriptRecorderState>>,
    health: Arc<RecorderHealth>,
    bus: EventBus,
}

impl TranscriptRecorderGuard {
    /// Record the user's prompt at the front of the run (ADR-36 §4). Flushes
    /// immediately so the prompt is durable even if the run is interrupted.
    pub(crate) async fn record_user_message(&self, content: String) {
        self.append_entries(&[TranscriptEntry::User { content }]).await;
    }

    /// Append entries through the recorder's in-order buffer and flush any
    /// settled prefix right away. Used for the run-start user prompt and the
    /// run-end assistant/completion entries so they are durably persisted
    /// before the recorder is stopped.
    pub(crate) async fn append_entries(&self, entries: &[TranscriptEntry]) {
        let batch = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.pending.extend_from_slice(entries);
            state.drain_settled_prefix()
        };
        flush_entries(&self.store, self.session_id, batch, &self.flush_cancel, &self.health).await;
    }

    /// Stop the recorder: cancel the background task, let it drain any events
    /// still in flight, then settle still-`Running` tool calls and flush
    /// everything remaining in order.
    pub(crate) async fn stop(self) -> RecorderOutcome {
        self.finish(&[]).await
    }

    /// Drain bus messages before appending the final response. Follow-ups and
    /// progress replies must precede the answer even if the recorder has not
    /// been scheduled since the coordinator published them.
    pub(crate) async fn finish(mut self, entries: &[TranscriptEntry]) -> RecorderOutcome {
        drain_task(&mut self.task, &self.health, &self.cancel).await;
        let batch = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.settle_running();
            state.pending.extend_from_slice(entries);
            std::mem::take(&mut state.pending)
        };
        flush_entries(&self.store, self.session_id, batch, &self.flush_cancel, &self.health).await;
        self.health.persist(&self.store, self.session_id, &self.bus, CancellationToken::new()).await
    }
}

impl Drop for TranscriptRecorderGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// ADR-58 P2+P3 (F8): resolve the review/validate gate labels for transcript
/// activity entries from the resolved blueprint's stage definitions.
///
/// The gate stage's configured `StageDef.label` is used directly (resolved by
/// KIND, so a renamed review/validate tag still surfaces its label — issue
/// #150); on the default `standard` blueprint those labels are the generic
/// "Review" / "Validate" strings. Without a resolved blueprint (tests,
/// `[orchestration]`-less configs) [`GateLabels::default`] supplies the same
/// generic fallback — never a role id.
pub(crate) fn gate_labels_for_resolved(resolved: Option<&ResolvedBlueprint>) -> GateLabels {
    let Some(resolved) = resolved else { return GateLabels::default() };
    let facade = BlueprintFacade::new(resolved);
    let mut labels = GateLabels::default();
    if let Some(stage) = facade.first_stage_of_kind(StageKind::Review) {
        labels.review = stage.def.label.clone();
    }
    if let Some(stage) = facade.first_stage_of_kind(StageKind::Acceptance) {
        labels.validate = stage.def.label.clone();
    }
    labels
}

pub(crate) fn start_transcript_recorder(
    bus: &EventBus,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    gate_labels: GateLabels,
) -> TranscriptRecorderGuard {
    let mut receiver = bus.subscribe_durable_for_session(session_id);
    let cancel = CancellationToken::new();
    let flush_cancel = CancellationToken::new();
    let state = Arc::new(Mutex::new(TranscriptRecorderState::new()));
    let task_cancel = cancel.clone();
    let task_flush_cancel = flush_cancel.clone();
    let task_state = state.clone();
    let task_store = store.clone();
    let task_labels = gate_labels;
    let health = Arc::new(RecorderHealth::new(RecorderKind::Transcript));
    let task_health = health.clone();
    let task = tokio::spawn(async move {
        let mut gap = GapLog::new("transcript-recorder", task_health.clone());
        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => {
                    // Freeze the queue before draining its retained suffix.
                    receiver.close();
                    while let Ok(event) = receiver.try_recv() {
                        gap.observe(&receiver, session_id);
                        handle_bus_event(event, session_id, &task_state, &task_store, &task_flush_cancel, &task_labels, &task_health).await;
                    }
                    gap.observe(&receiver, session_id);
                    break;
                }
                received = receiver.recv() => match received {
                    Some(event) => {
                        gap.observe(&receiver, session_id);
                        handle_bus_event(event, session_id, &task_state, &task_store, &task_flush_cancel, &task_labels, &task_health).await;
                    }
                    None => {
                        gap.observe(&receiver, session_id);
                        break;
                    }
                }
            }
        }
    });
    TranscriptRecorderGuard {
        cancel,
        flush_cancel,
        task: Some(task),
        store,
        session_id,
        state,
        health,
        bus: bus.clone(),
    }
}

#[async_trait]
impl AuditLog for NoopAuditLog {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}
