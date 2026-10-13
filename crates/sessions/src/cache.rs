//! One reporting contract for persisted provider telemetry in desktop, CLI and evals.

use crate::{SessionError, SessionStore};
use concerto_core::event::EventKind;
use concerto_core::ids::Ulid;
use concerto_core::provider_call::{ProviderCallRecord, ProviderCallSummary};
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheReport {
    pub schema_version: u32,
    pub session_id: Ulid,
    pub recorded_events: usize,
    pub unfinished_attempts: u64,
    pub token_hit_rate: Option<f64>,
    pub request_hit_rate: Option<f64>,
    pub measurement_coverage: Option<f64>,
    pub summary: ProviderCallSummary,
    pub by_provider_model: BTreeMap<String, ProviderCallSummary>,
    pub attempts: Vec<ProviderCallRecord>,
}

impl CacheReport {
    pub fn from_events(
        session_id: Ulid,
        events: &[crate::replay::StoredEvent],
    ) -> Result<Self, SessionError> {
        let mut attempts = BTreeMap::new();
        for event in events {
            if !matches!(event.event_kind.as_str(), "ProviderCallStarted" | "ProviderCallFinished")
            {
                continue;
            }
            match event.to_event()?.kind {
                EventKind::ProviderCallStarted { mut record } => {
                    record.status = "unfinished".into();
                    attempts.entry(record.attempt_id).or_insert(record);
                }
                EventKind::ProviderCallFinished { record } => {
                    attempts.insert(record.attempt_id, record);
                }
                _ => {}
            }
        }
        let attempts: Vec<ProviderCallRecord> = attempts.into_values().collect();
        let mut report = Self {
            schema_version: 1,
            session_id,
            recorded_events: events.len(),
            unfinished_attempts: 0,
            token_hit_rate: None,
            request_hit_rate: None,
            measurement_coverage: None,
            summary: ProviderCallSummary::default(),
            by_provider_model: BTreeMap::new(),
            attempts,
        };
        for call in &report.attempts {
            if call.session_id != session_id || call.schema_version != 1 {
                return Err(SessionError::Validation(
                    "provider telemetry session/schema mismatch".into(),
                ));
            }
            report.unfinished_attempts += u64::from(call.status == "unfinished");
            report.summary.record(call);
            report
                .by_provider_model
                .entry(format!("{} / {}", call.provider, call.model))
                .or_default()
                .record(call);
        }
        report.token_hit_rate = report.summary.cache.token_hit_rate();
        report.request_hit_rate = report.summary.cache.request_hit_rate();
        report.measurement_coverage = report.summary.cache.measurement_coverage();
        Ok(report)
    }

    pub fn caption(&self) -> String {
        let percent = |value: Option<f64>| {
            value.map(|v| format!("{:.1}%", v * 100.0)).unwrap_or_else(|| "N/A".into())
        };
        format!("Prompt cache: {} token reuse · {} coverage · {} attempts · {} missing usage · {} unfinished",
            percent(self.summary.cache.token_hit_rate()), percent(self.summary.cache.measurement_coverage()),
            self.summary.cache.calls, self.summary.missing_usage_calls, self.unfinished_attempts)
    }
}

pub async fn load_cache_report(
    store: &dyn SessionStore,
    session_id: Ulid,
    cancel: CancellationToken,
) -> Result<CacheReport, SessionError> {
    let events = store.load_events(session_id, cancel).await?;
    CacheReport::from_events(session_id, &events)
}
