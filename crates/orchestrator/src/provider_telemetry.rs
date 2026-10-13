//! Attempts are recorded even when an awaiting future is dropped.

use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::provider_call::ProviderCallRecord;
use concerto_core::types::{CompletionChunk, CompletionRequest, Role, TaskId};
use std::time::Instant;

pub(crate) struct AttemptRecorder {
    bus: EventBus,
    start: Instant,
    record: ProviderCallRecord,
    published: bool,
}

impl AttemptRecorder {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        bus: EventBus,
        session_id: Ulid,
        task_id: TaskId,
        request_id: Ulid,
        attempt: u32,
        provider: &str,
        request: &CompletionRequest,
    ) -> Self {
        let fingerprint = |value: &serde_json::Value| {
            blake3::hash(value.to_string().as_bytes()).to_hex().to_string()
        };
        let system: Vec<_> =
            request.messages.iter().filter(|message| message.role == Role::System).collect();
        let history: Vec<_> =
            request.messages.iter().filter(|message| message.role != Role::System).collect();
        let recorder = Self {
            bus,
            start: Instant::now(),
            published: false,
            record: ProviderCallRecord {
                schema_version: 1,
                request_id,
                attempt_id: Ulid::new(),
                attempt,
                session_id,
                task_id,
                provider: provider.into(),
                model: request.model.clone(),
                system_fingerprint: fingerprint(&serde_json::json!(system)),
                history_fingerprint: fingerprint(&serde_json::json!(history)),
                tools_fingerprint: fingerprint(&serde_json::json!(request.tools)),
                status: "interrupted".into(),
                elapsed_ms: 0,
                first_content_ms: None,
                usage: None,
                estimated_cost_usd: None,
                cost_source: "unknown".into(),
            },
        };
        let _ = recorder.bus.publish_durable_for_session(
            session_id,
            task_id.0,
            EventKind::ProviderCallStarted { record: recorder.record.clone() },
        );
        recorder
    }

    pub(crate) fn observe(&mut self, chunk: &CompletionChunk) {
        if !chunk.delta.is_empty() && self.record.first_content_ms.is_none() {
            self.record.first_content_ms = Some(self.start.elapsed().as_millis() as u64);
        }
        if chunk.is_final {
            self.record.usage = chunk.usage;
        }
    }

    pub(crate) fn finish(
        &mut self,
        status: &str,
        provider: &dyn concerto_core::traits::LlmProvider,
    ) {
        self.record.status = status.into();
        if let Some((input, output)) =
            self.record.usage.and_then(|usage| usage.prompt_tokens.zip(usage.completion_tokens))
        {
            let cost = provider.approximate_cost(input, output);
            if cost.is_finite() && cost >= 0.0 {
                self.record.estimated_cost_usd = Some(cost);
                // Existing approximate_cost rates do not price cache discounts.
                self.record.cost_source = "legacy_nominal_estimate".into();
            }
        }
        self.publish();
    }

    fn publish(&mut self) {
        if self.published {
            return;
        }
        self.published = true;
        self.record.elapsed_ms = self.start.elapsed().as_millis() as u64;
        if let Err(error) = self.bus.publish_durable_for_session(
            self.record.session_id,
            self.record.task_id.0,
            EventKind::ProviderCallFinished { record: self.record.clone() },
        ) {
            tracing::warn!(%error, "provider-attempt telemetry could not be published");
        }
    }
}

impl Drop for AttemptRecorder {
    fn drop(&mut self) {
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_attempt_records_interruption_once_without_fabricated_usage() {
        let bus = EventBus::new(1);
        let mut durable = bus.subscribe_durable();
        let session = Ulid::new();
        let task = TaskId::new();
        let request = CompletionRequest { model: "fixture".into(), ..Default::default() };
        let recorder =
            AttemptRecorder::new(bus, session, task, Ulid::new(), 1, "fixture", &request);
        drop(recorder);
        let started = durable.try_recv().unwrap();
        let finished = durable.try_recv().unwrap();
        let EventKind::ProviderCallStarted { record: start } = &started.kind else {
            panic!("start missing")
        };
        let EventKind::ProviderCallFinished { record: end } = &finished.kind else {
            panic!("finish missing")
        };
        assert_eq!(start.attempt_id, end.attempt_id);
        assert_eq!(end.status, "interrupted");
        assert!(end.usage.is_none());
        assert!(end.estimated_cost_usd.is_none());
        assert!(durable.try_recv().is_err());
    }
}
