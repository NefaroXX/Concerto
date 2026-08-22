//! Spend chip + cap-state helpers for the status bar and Spend Log modal.
//!
//! Pure presentation logic — no iced widgets — so the threshold rules are
//! unit-testable without constructing an `App` or touching the renderer.

use concerto_sessions::spend::SpendRecord;
use time::OffsetDateTime;

/// Thresholds (percent of the session cap) mirrored from
/// `SpendAccumulator::check_cap` in `concerto_sessions::spend`.
const APPROACHING_PCT: f64 = 80.0;
const EXCEEDED_PCT: f64 = 100.0;

/// Cap signal derived from the latest spend event. `Normal` is the default;
/// a fresh session starts under cap until an `Approaching`/`Exceeded` event
/// arrives (the chip's own percentage computation also guards the color).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CapUiState {
    #[default]
    Normal,
    Approaching {
        current_usd: f64,
        cap_usd: f64,
        pct: f64,
    },
    Exceeded {
        current_usd: f64,
        cap_usd: f64,
    },
}

/// Which palette tone the status-bar spend chip renders with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendChipTone {
    Normal,
    Warning,
    Danger,
}

/// Computed presentation for the status-bar spend chip.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendChipStyle {
    pub tone: SpendChipTone,
    pub label: String,
}

/// Decide the spend chip's tone + label from the live session total, the
/// configured session cap and the latest cap-event state.
///
/// An `Exceeded` cap state wins even when the current total would not
/// independently cross 100% (e.g. the cap was lowered mid-session).
pub fn spend_chip_state(total: f64, cap: Option<f64>, cap_state: &CapUiState) -> SpendChipStyle {
    let label = format!("◷ ${total:.3}");
    let tone = if matches!(cap_state, CapUiState::Exceeded { .. }) {
        SpendChipTone::Danger
    } else {
        match pct_of_cap(total, cap) {
            Some(pct) if pct >= EXCEEDED_PCT => SpendChipTone::Danger,
            Some(pct) if pct >= APPROACHING_PCT => SpendChipTone::Warning,
            _ => SpendChipTone::Normal,
        }
    };
    SpendChipStyle { tone, label }
}

/// Percentage of the session cap the total currently uses, or `None` when no
/// positive cap is configured.
pub fn pct_of_cap(total: f64, cap: Option<f64>) -> Option<f64> {
    match cap {
        Some(cap) if cap > 0.0 => Some((total / cap) * 100.0),
        _ => None,
    }
}

/// Cap status text for the Spend Log modal header.
pub fn cap_status_text(cap_state: &CapUiState, cap: Option<f64>) -> String {
    match cap_state {
        CapUiState::Normal => {
            if cap.is_some_and(|cap| cap > 0.0) {
                "under cap".to_string()
            } else {
                "no cap".to_string()
            }
        }
        CapUiState::Approaching { pct, .. } => format!("approaching cap ({pct:.0}%)"),
        CapUiState::Exceeded { .. } => "cap exceeded".to_string(),
    }
}

/// Compact timestamp for spend-log rows, e.g. `08-03 14:22`. Spend records
/// are persisted as UTC; the workspace `time` feature set has no
/// `local-offset`, so rows show UTC rather than the viewer's local zone.
pub fn compact_created_at(dt: OffsetDateTime) -> String {
    let Ok(desc) = time::format_description::parse("[month]-[day] [hour]:[minute]") else {
        return String::new();
    };
    dt.format(&desc).unwrap_or_else(|_| String::new())
}

/// Session totals aggregated from the spend log (modal header).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpendTotals {
    pub total_cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub record_count: usize,
}

/// Sum a session's spend records into header totals.
pub fn spend_totals(records: &[SpendRecord]) -> SpendTotals {
    records.iter().fold(SpendTotals::default(), |mut totals, record| {
        totals.total_cost_usd += record.cost_usd;
        totals.tokens_in += record.tokens_in;
        totals.tokens_out += record.tokens_out;
        totals.record_count += 1;
        totals
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;

    fn style(total: f64, cap: Option<f64>, cap_state: &CapUiState) -> SpendChipStyle {
        spend_chip_state(total, cap, cap_state)
    }

    fn record(cost_usd: f64, tokens_in: u64, tokens_out: u64) -> SpendRecord {
        SpendRecord {
            id: Ulid::new(),
            session_id: Ulid::new(),
            task_id: None,
            provider: "openrouter".into(),
            model: "m/a".into(),
            tokens_in,
            tokens_out,
            cost_usd,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn no_cap_renders_normal_style() {
        let result = style(0.5, None, &CapUiState::Normal);
        assert_eq!(result.tone, SpendChipTone::Normal);
        assert_eq!(result.label, "◷ $0.500");
    }

    #[test]
    fn below_threshold_is_normal() {
        let result = style(0.79, Some(1.0), &CapUiState::Normal);
        assert_eq!(result.tone, SpendChipTone::Normal);
    }

    #[test]
    fn exactly_threshold_is_warning() {
        let result = style(0.80, Some(1.0), &CapUiState::Normal);
        assert_eq!(result.tone, SpendChipTone::Warning);
    }

    #[test]
    fn over_threshold_is_danger() {
        let result = style(1.00, Some(1.0), &CapUiState::Normal);
        assert_eq!(result.tone, SpendChipTone::Danger);
    }

    #[test]
    fn exceeded_cap_state_is_danger_even_with_low_total() {
        // A cap lowered mid-session: the total is well under the (old) 100%,
        // but the latest event said Exceeded, so the chip stays red.
        let exceeded = CapUiState::Exceeded { current_usd: 0.4, cap_usd: 0.5 };
        let result = style(0.4, Some(0.5), &exceeded);
        assert_eq!(result.tone, SpendChipTone::Danger);
    }

    #[test]
    fn approaching_cap_state_is_warning() {
        let approaching = CapUiState::Approaching { current_usd: 0.9, cap_usd: 1.0, pct: 90.0 };
        let result = style(0.9, Some(1.0), &approaching);
        assert_eq!(result.tone, SpendChipTone::Warning);
    }

    #[test]
    fn zero_cap_is_treated_as_no_cap() {
        assert_eq!(pct_of_cap(1.0, Some(0.0)), None);
        assert_eq!(pct_of_cap(1.0, Some(-1.0)), None);
    }

    #[test]
    fn zero_total_formats_with_three_decimals() {
        let result = style(0.0, None, &CapUiState::Normal);
        assert_eq!(result.label, "◷ $0.000");
    }

    #[test]
    fn cap_status_text_covers_all_states() {
        assert_eq!(cap_status_text(&CapUiState::Normal, Some(1.0)), "under cap");
        assert_eq!(cap_status_text(&CapUiState::Normal, None), "no cap");
        assert_eq!(
            cap_status_text(
                &CapUiState::Approaching { current_usd: 0.9, cap_usd: 1.0, pct: 90.0 },
                Some(1.0)
            ),
            "approaching cap (90%)"
        );
        assert_eq!(
            cap_status_text(&CapUiState::Exceeded { current_usd: 1.1, cap_usd: 1.0 }, Some(1.0)),
            "cap exceeded"
        );
    }

    #[test]
    fn spend_totals_aggregate_records() {
        let records = vec![record(0.01, 100, 50), record(0.02, 200, 80)];
        let totals = spend_totals(&records);
        assert_eq!(totals.total_cost_usd, 0.03);
        assert_eq!(totals.tokens_in, 300);
        assert_eq!(totals.tokens_out, 130);
        assert_eq!(totals.record_count, 2);
    }

    #[test]
    fn compact_created_at_formats_month_day_hour_minute() {
        let dt = OffsetDateTime::parse(
            "2026-08-03T14:22:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("valid RFC3339 timestamp");
        assert_eq!(compact_created_at(dt), "08-03 14:22");
    }
}
