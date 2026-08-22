use concerto_core::types::ProviderMetrics;
use std::time::Instant;

/// Records timing and token usage for a provider call.
#[derive(Debug)]
pub struct MetricsRecorder {
    provider: String,
    model: String,
    tokens_in: u64,
    tokens_out: u64,
    start: Instant,
    cost_usd: f64,
}

impl MetricsRecorder {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            tokens_in: 0,
            tokens_out: 0,
            start: Instant::now(),
            cost_usd: 0.0,
        }
    }

    pub fn record_tokens_in(&mut self, n: u64) {
        self.tokens_in = self.tokens_in.saturating_add(n);
    }

    pub fn record_tokens_out(&mut self, n: u64) {
        self.tokens_out = self.tokens_out.saturating_add(n);
    }

    pub fn set_cost(&mut self, cost: f64) {
        self.cost_usd = cost;
    }

    /// Returns the number of input tokens recorded so far.
    pub fn tokens_in(&self) -> u64 {
        self.tokens_in
    }

    /// Returns the number of output tokens recorded so far.
    pub fn tokens_out(&self) -> u64 {
        self.tokens_out
    }

    pub fn finish(&self) -> ProviderMetrics {
        ProviderMetrics {
            provider: self.provider.clone(),
            model: self.model.clone(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cost_usd: self.cost_usd,
            latency_ms: self.start.elapsed().as_millis() as u64,
        }
    }
}
