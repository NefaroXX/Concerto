//! Shared prompt-cache reporting and explicit, versioned price estimates.
//! Missing provider measurements never become measured zeroes.

use crate::types::CompletionUsage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheUsageSummary {
    pub calls: u64,
    pub cache_reporting_calls: u64,
    pub rate_eligible_calls: u64,
    pub calls_with_hits: u64,
    pub invalid_cache_calls: u64,
    pub measured_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: Option<u64>,
}

impl CacheUsageSummary {
    pub fn record(&mut self, usage: Option<CompletionUsage>) {
        self.calls += 1;
        let Some(usage) = usage else { return };
        if let Some(writes) = usage.cache_write_tokens {
            self.cache_write_tokens = Some(self.cache_write_tokens.unwrap_or(0) + writes);
        }
        let Some(reads) = usage.cache_read_tokens else { return };
        self.cache_reporting_calls += 1;
        if let Some(total) = usage.prompt_tokens {
            if reads > total {
                self.invalid_cache_calls += 1;
                return;
            }
            self.rate_eligible_calls += 1;
            self.measured_input_tokens += total;
            self.cache_read_tokens += reads;
        }
        self.calls_with_hits += u64::from(reads > 0);
    }

    /// Ratio of sums over the same set of valid, measured requests.
    pub fn token_hit_rate(&self) -> Option<f64> {
        (self.measured_input_tokens > 0)
            .then(|| self.cache_read_tokens as f64 / self.measured_input_tokens as f64)
    }

    pub fn request_hit_rate(&self) -> Option<f64> {
        let valid = self.cache_reporting_calls - self.invalid_cache_calls;
        (valid > 0).then(|| self.calls_with_hits as f64 / valid as f64)
    }

    pub fn measurement_coverage(&self) -> Option<f64> {
        (self.calls > 0).then(|| self.cache_reporting_calls as f64 / self.calls as f64)
    }
}

/// Caller supplies effective model prices and their version; no guessed
/// discount is applied to unknown gateway/model routes. All rates are USD/MTok.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePricing {
    pub version: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
}

impl CachePricing {
    /// A complete estimate requires every charged component. Reasoning is
    /// already included in output; cache TTL counts are subsets of writes.
    pub fn estimate(&self, usage: CompletionUsage) -> Option<f64> {
        fn priced(tokens: u64, rate: Option<f64>) -> Option<f64> {
            if tokens == 0 {
                return Some(0.0);
            }
            let rate = rate?;
            (rate.is_finite() && rate >= 0.0).then_some(tokens as f64 * rate)
        }
        let input = usage.uncached_prompt_tokens?;
        let reads = usage.cache_read_tokens?;
        let writes = usage.cache_write_tokens?;
        let total = input.checked_add(reads)?.checked_add(writes)?;
        if usage.prompt_tokens != Some(total) {
            return None;
        }
        let write_cost = match (usage.cache_write_5m_tokens, usage.cache_write_1h_tokens) {
            (Some(short), Some(long)) => {
                if short.checked_add(long)? != writes {
                    return None;
                }
                priced(short, self.cache_write_5m)? + priced(long, self.cache_write_1h)?
            }
            (None, None) => priced(writes, self.cache_write)?,
            _ => return None,
        };
        let result = (priced(input, Some(self.input))?
            + priced(reads, self.cache_read)?
            + write_cost
            + priced(usage.completion_tokens?, Some(self.output))?)
            / 1_000_000.0;
        result.is_finite().then_some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_use_matched_token_sums_and_expose_unknown_coverage() {
        let mut report = CacheUsageSummary::default();
        report.record(None);
        report.record(Some(CompletionUsage {
            prompt_tokens: Some(100),
            cache_read_tokens: Some(90),
            ..Default::default()
        }));
        report.record(Some(CompletionUsage {
            prompt_tokens: Some(900),
            cache_read_tokens: Some(0),
            ..Default::default()
        }));
        assert_eq!(report.token_hit_rate(), Some(0.09));
        assert_eq!(report.request_hit_rate(), Some(0.5));
        assert_eq!(report.measurement_coverage(), Some(2.0 / 3.0));
        assert_eq!(report.cache_write_tokens, None);
        assert_eq!(CacheUsageSummary::default().token_hit_rate(), None);
    }

    #[test]
    fn invalid_counts_are_excluded_and_reported() {
        let mut report = CacheUsageSummary::default();
        report.record(Some(CompletionUsage {
            prompt_tokens: Some(10),
            cache_read_tokens: Some(11),
            ..Default::default()
        }));
        assert_eq!(report.invalid_cache_calls, 1);
        assert_eq!(report.token_hit_rate(), None);
        assert_eq!(report.request_hit_rate(), None);
    }

    #[test]
    fn cost_prices_subsets_once_and_refuses_missing_measurements() {
        let pricing = CachePricing {
            version: "fixture-v1".into(),
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
            cache_write_5m: Some(3.75),
            cache_write_1h: Some(6.0),
        };
        let usage = CompletionUsage {
            prompt_tokens: Some(1000),
            completion_tokens: Some(100),
            uncached_prompt_tokens: Some(100),
            cache_read_tokens: Some(600),
            cache_write_tokens: Some(300),
            cache_write_5m_tokens: Some(200),
            cache_write_1h_tokens: Some(100),
            reasoning_tokens: Some(50),
            ..CompletionUsage::default()
        };
        assert!((pricing.estimate(usage).unwrap() - 0.00333).abs() < 1e-10);
        assert_eq!(pricing.estimate(CompletionUsage { cache_read_tokens: None, ..usage }), None);
    }
}
