//! Content-free provider attempts, joined to session/task and audit evidence.

use crate::cache_usage::CacheUsageSummary;
use crate::ids::Ulid;
use crate::types::{CompletionUsage, TaskId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCallRecord {
    pub schema_version: u32,
    pub request_id: Ulid,
    pub attempt_id: Ulid,
    pub attempt: u32,
    pub session_id: Ulid,
    pub task_id: TaskId,
    pub provider: String,
    pub model: String,
    /// Fingerprints describe changes, not proof of a provider cache miss.
    pub system_fingerprint: String,
    pub history_fingerprint: String,
    pub tools_fingerprint: String,
    pub status: String,
    pub elapsed_ms: u64,
    pub first_content_ms: Option<u64>,
    pub usage: Option<CompletionUsage>,
    pub estimated_cost_usd: Option<f64>,
    pub cost_source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCallSummary {
    pub cache: CacheUsageSummary,
    pub failed_attempts: u64,
    pub interrupted_attempts: u64,
    pub missing_usage_calls: u64,
    pub missing_cost_calls: u64,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
    /// Known subtotal; never a complete cost when missing_cost_calls > 0.
    pub known_estimated_cost_usd: f64,
    #[serde(default)]
    pub first_content_ms: Vec<u64>,
    #[serde(default)]
    pub elapsed_ms: Vec<u64>,
}

impl ProviderCallSummary {
    pub fn record(&mut self, call: &ProviderCallRecord) {
        self.elapsed_ms.push(call.elapsed_ms);
        if let Some(time) = call.first_content_ms {
            self.first_content_ms.push(time);
        }
        self.cache.record(call.usage);
        self.failed_attempts += u64::from(call.status == "error");
        self.interrupted_attempts +=
            u64::from(matches!(call.status.as_str(), "interrupted" | "cancelled" | "unfinished"));
        match call.usage {
            Some(usage) => {
                self.reported_input_tokens += usage.prompt_tokens.unwrap_or(0);
                self.reported_output_tokens += usage.completion_tokens.unwrap_or(0);
                self.missing_usage_calls +=
                    u64::from(usage.prompt_tokens.is_none() || usage.completion_tokens.is_none());
            }
            None => self.missing_usage_calls += 1,
        }
        match call.estimated_cost_usd.filter(|cost| cost.is_finite() && *cost >= 0.0) {
            Some(cost) => self.known_estimated_cost_usd += cost,
            None => self.missing_cost_calls += 1,
        }
    }

    pub fn complete_estimated_cost_usd(&self) -> Option<f64> {
        (self.cache.calls > 0 && self.missing_cost_calls == 0)
            .then_some(self.known_estimated_cost_usd)
    }
}
