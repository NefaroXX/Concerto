//! ADR-69 slice 2 — link evidence scoring.
//!
//! Pure, deterministic scoring of a chunk's INCOMING links into a 0–10
//! evidence score. 0 means replaceable / unsupported; 10 means
//! strongly supported. The score is computed from:
//!
//! - the link **kind** (through [`kind_weight`], ADR's `supports >
//!   references > extends` ordering; supersession / merge / contradiction
//!   demote the chunk they point at),
//! - the link **weight** (clamped to `0..=1`),
//! - the link's **age** (exponential decay over a window, with an exact 0
//!   past the window — ADR-69 A5 decay floor).
//!
//! Consumers:
//! - [`crate::rag::retrieve_with_cascade`] re-ranks fused results by
//!   `rank + γ·(10 − score)`.
//! - [`crate::ttl::prune_orphaned_derived`] deletes derived rows whose
//!   score falls at or below [`ORPHAN_TRIM_SCORE`].
//!
//! This module has no clocks and no I/O: callers pass `now_unix` (seconds)
//! and the store hands over already-decoded [`TimedLink`]s.

use concerto_core::memory::{MemoryLink, MemoryLinkKind};
use time::OffsetDateTime;

/// Maximum link score — the ceiling both the cascade reorder and the score
/// clamp scale against.
pub const LINK_SCORE_MAX: f64 = 10.0;

/// Score at or below which [`crate::ttl::prune_orphaned_derived`] treats a
/// derived row as orphaned evidence and deletes it.
pub const ORPHAN_TRIM_SCORE: f64 = 0.5;

/// Default decay window (days) behind which evidence is discarded. Used by
/// [`LinkCascadeConfig::default`] and the runtime wiring when the user has
/// not configured a window (ADR-69 A5).
pub const DECAY_FLOOR_DAYS: i64 = 90;

/// In-degree at which the breadth component of a score saturates: once a
/// chunk is supported by this many links, more links no longer add breadth
/// (bounded, saturating evidence — never unbounded).
pub const DEGREE_SATURATION: usize = 4;

/// One incoming link plus its creation moment, as decoded from the
/// `memory_links.created_at` string.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedLink {
    pub link: MemoryLink,
    /// Unix seconds (UTC) at which the link was created.
    pub created_unix: i64,
}

impl TimedLink {
    /// Decode a link row. The creation timestamp is stored as
    /// [`OffsetDateTime::to_string()`]; unparseable rows return `None` so
    /// callers can skip them fail-open.
    pub fn from_db(link: MemoryLink, created_at: &str) -> Option<Self> {
        Some(Self { link, created_unix: parse_db_timestamp(created_at)?.unix_timestamp() })
    }
}

/// Parse the timestamp string [`OffsetDateTime::to_string()`] stores in
/// `created_at` columns: `YYYY-MM-DD H:MM:SS[.fffffffff] ±HH:MM:SS`.
///
/// The hour is unpadded (1–2 digits), the fraction carries 1–9 digits (an
/// exact `.0` for whole seconds), and the offset is always present but
/// tolerated absent so older rows still parse. Returns `None` for anything
/// that does not match — callers fail open.
pub fn parse_db_timestamp(timestamp: &str) -> Option<OffsetDateTime> {
    let mut parts = timestamp.splitn(3, ' ');
    let date_part = parts.next()?;
    let time_part = parts.next()?;
    let offset_part = parts.next();

    let (year, month, day) = parse_db_date(date_part)?;
    let (hour, minute, second, nanosecond) = parse_db_time(time_part)?;
    let offset = match offset_part {
        Some(offset) => parse_db_offset(offset)?,
        None => time::UtcOffset::UTC,
    };

    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let time = time::Time::from_hms_nano(hour, minute, second, nanosecond).ok()?;
    Some(date.with_time(time).assume_offset(offset))
}

/// `YYYY-MM-DD` → `(year, month, day)`.
fn parse_db_date(date: &str) -> Option<(i32, time::Month, u8)> {
    let mut fields = date.split('-');
    let year: i32 = fields.next()?.parse().ok()?;
    let month = time::Month::try_from(fields.next()?.parse::<u8>().ok()?).ok()?;
    let day: u8 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((year, month, day))
}

/// `H:MM:SS[.fffffffff]` → `(hour, minute, second, nanosecond)`.
fn parse_db_time(time: &str) -> Option<(u8, u8, u8, u32)> {
    let mut fields = time.split(':');
    let hour: u8 = fields.next()?.parse().ok()?;
    let minute: u8 = fields.next()?.parse().ok()?;
    let second_and_fraction = fields.next()?;
    if fields.next().is_some() {
        return None;
    }

    let (second, nanosecond) = match second_and_fraction.split_once('.') {
        Some((seconds, fraction)) => {
            let second: u8 = seconds.parse().ok()?;
            let fraction = fraction.as_bytes();
            if fraction.is_empty() || fraction.len() > 9 {
                return None;
            }
            // Right-align the fraction to nanoseconds: `.123` → 123_000_000.
            let mut nanosecond: u32 = 0;
            for (position, byte) in fraction.iter().enumerate() {
                let digit = u32::from(byte - b'0');
                if digit > 9 {
                    return None;
                }
                nanosecond += digit * 10u32.pow((9 - position - 1) as u32);
            }
            (second, nanosecond)
        }
        None => (second_and_fraction.parse().ok()?, 0),
    };
    Some((hour, minute, second, nanosecond))
}

/// `±HH:MM:SS` (always 9 bytes including the sign) → a UTC offset.
fn parse_db_offset(offset: &str) -> Option<time::UtcOffset> {
    if offset.len() != 9 {
        return None;
    }
    let sign = match offset.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hour: i32 = offset.get(1..3)?.parse().ok()?;
    let minute: i32 = offset.get(4..6)?.parse().ok()?;
    let second: i32 = offset.get(7..9)?.parse().ok()?;
    time::UtcOffset::from_whole_seconds(sign * (hour * 3600 + minute * 60 + second)).ok()
}

/// Evidence weight of a link kind, implementing the ADR-69 ordering
/// `supports > references > extends`. Supersession, merge, and
/// contradiction are NEGATIVE evidence for the chunk they point at
/// (a chunk that other chunks supersede or contradict is less authoritative).
pub fn kind_weight(kind: MemoryLinkKind) -> f64 {
    match kind {
        MemoryLinkKind::Supports => 1.0,
        MemoryLinkKind::References => 0.6,
        MemoryLinkKind::Extends => 0.4,
        MemoryLinkKind::Contradicts => -0.6,
        MemoryLinkKind::Supersedes => -0.8,
        MemoryLinkKind::Merges => -0.3,
        _ => 0.0, // future kinds stay neutral until assigned a weight
    }
}

/// Decay multiplier for a link of `age_days`:
///
/// - `decay_window_days` `None` or `Some(0)` → decay disabled (1.0 always).
/// - age ≤ 0 (fresh or clock skew) → 1.0.
/// - age ≥ window → 0.0 (ADR-69 A5 decay floor: beyond the window there is
///   no evidence at all, not a tiny residue).
/// - otherwise → `exp(-age / window)`.
pub fn decay_factor(age_days: f64, decay_window_days: Option<u16>) -> f64 {
    let window = match decay_window_days {
        None | Some(0) => return 1.0,
        Some(window) => f64::from(window),
    };
    if age_days <= 0.0 {
        return 1.0;
    }
    if age_days >= window {
        return 0.0;
    }
    (-age_days / window).exp()
}

/// ADR-69 slice-2 evidence score of a chunk from its INCOMING links,
/// clamped to `0..=LINK_SCORE_MAX`.
///
/// evidence = Σ kind_weight(kind)·weight·decay_factor(age, window)
/// breadth  = min(#links with a positive evidence contribution, DEGREE_SATURATION)
///             / DEGREE_SATURATION              (saturating in-degree)
/// score    = (0.75·evidence/(1+|evidence|) + 0.25·breadth)·10
///
/// Evidence dominates; breadth adds a bounded lift once multiple links
/// corroborate. `now_unix` is the caller's clock (seconds, UTC).
pub fn chunk_link_score(links: &[TimedLink], decay_window_days: Option<u16>, now_unix: i64) -> f64 {
    let days_per_second = 1.0 / 86_400.0;
    let mut evidence: f64 = 0.0;
    let mut supportive_links: usize = 0;

    for timed in links {
        let age_days = (now_unix - timed.created_unix) as f64 * days_per_second;
        let contribution = kind_weight(timed.link.kind)
            * timed.link.weight.clamp(0.0, 1.0)
            * decay_factor(age_days, decay_window_days);
        if contribution > 0.0 {
            supportive_links += 1;
        }
        evidence += contribution;
    }

    let breadth = (supportive_links.min(DEGREE_SATURATION) as f64) / (DEGREE_SATURATION as f64);
    let score = (0.75 * (evidence / (1.0 + evidence.abs())) + 0.25 * breadth) * LINK_SCORE_MAX;
    score.clamp(0.0, LINK_SCORE_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::MemoryLink;
    use time::OffsetDateTime;

    fn timed(link: MemoryLink, now_minus_days: i64) -> TimedLink {
        let created = OffsetDateTime::now_utc() - time::Duration::days(now_minus_days);
        TimedLink { link, created_unix: created.unix_timestamp() }
    }

    fn support(to: &str, weight: f64) -> MemoryLink {
        MemoryLink { from: "source".into(), to: to.into(), kind: MemoryLinkKind::Supports, weight }
    }

    #[test]
    fn parse_db_timestamp_round_trips_display_format() {
        // The exact shapes OffsetDateTime::to_string() emits (verified from
        // the time crate's Display impl): unpadded hour, fraction always
        // present, offset always present.
        for raw in [
            "2023-11-14 22:13:20.0 +00:00:00",
            "2023-11-14 1:02:03.0 +05:45:30",
            "1916-02-18 1:46:40.0 -11:00:00",
            "2026-09-18 12:34:56.123 +00:00:00",
            "2026-09-18 12:34:56.123456789 +00:00:00",
        ] {
            let parsed = parse_db_timestamp(raw).unwrap_or_else(|| panic!("parse {raw:?}"));
            assert_eq!(parsed.to_string(), raw, "round-trip must be exact");
        }
    }

    #[test]
    fn parse_db_timestamp_accepts_plain_utc() {
        // Rows written before the offset format was guaranteed still parse.
        let parsed = parse_db_timestamp("2023-11-14 22:13:20").expect("plain timestamp");
        let expected = time::Date::from_calendar_date(2023, time::Month::November, 14)
            .unwrap()
            .with_time(time::Time::from_hms(22, 13, 20).unwrap())
            .assume_utc();
        assert_eq!(parsed, expected);
        assert_eq!(parsed.to_string(), "2023-11-14 22:13:20.0 +00:00:00");
    }

    #[test]
    fn parse_db_timestamp_rejects_garbage() {
        for raw in [
            "",
            "nope",
            "2023-11-14",
            "22:13:20.0 +00:00:00",
            "2023-13-40 22:13:20",
            "2023-11-14 25:13:20",
            "2023-11-14 22:13:20.1234567890 +00:00:00",
            "2023-11-14 22:13:20 +00:00",
        ] {
            assert!(parse_db_timestamp(raw).is_none(), "must reject {raw:?}");
        }
    }

    #[test]
    fn kind_weights_follow_adr_ordering() {
        let supports = kind_weight(MemoryLinkKind::Supports);
        let references = kind_weight(MemoryLinkKind::References);
        let extends = kind_weight(MemoryLinkKind::Extends);
        assert!(supports > references && references > extends);
        assert!(kind_weight(MemoryLinkKind::Contradicts) < 0.0);
        assert!(kind_weight(MemoryLinkKind::Supersedes) < 0.0);
        assert!(kind_weight(MemoryLinkKind::Merges) < 0.0);
        assert!((kind_weight(MemoryLinkKind::References) - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn decay_factor_floors_at_window_and_disables_on_zero() {
        assert_eq!(decay_factor(1000.0, Some(90)), 0.0, "A5 floor: beyond window is exact 0");
        assert_eq!(decay_factor(90.0, Some(90)), 0.0, "at the window boundary is exact 0");
        assert_eq!(decay_factor(0.0, Some(90)), 1.0);
        assert_eq!(decay_factor(-5.0, Some(90)), 1.0, "clock skew is full strength");
        assert_eq!(decay_factor(1000.0, None), 1.0, "no window = no decay");
        assert_eq!(decay_factor(1000.0, Some(0)), 1.0, "explicit 0 = disabled");
        let mid = decay_factor(45.0, Some(90));
        assert!((0.0..1.0).contains(&mid) && mid > 0.5, "mid-window decays strictly");
    }

    #[test]
    fn score_boundaries_zero_and_max() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert_eq!(chunk_link_score(&[], Some(90), now), 0.0, "no links = replaceable");
        // A single fully-decayed support contributes nothing.
        let stale = TimedLink { link: support("c", 1.0), created_unix: now - 200 * 86_400 };
        assert_eq!(chunk_link_score(&[stale], Some(90), now), 0.0);
        // Heavy fresh support approaches — but never exceeds — the max.
        let mut links: Vec<TimedLink> = (0..8).map(|_| timed(support("c", 1.0), 0)).collect();
        links.push(TimedLink {
            link: {
                let mut l = support("c", 1.0);
                l.weight = 3.0;
                l
            },
            created_unix: now,
        });
        let score = chunk_link_score(&links, Some(90), now);
        assert!((8.0..=LINK_SCORE_MAX).contains(&score), "strong support is high, got {score}");
    }

    #[test]
    fn score_orders_by_evidence_strength() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let single_support = chunk_link_score(&[timed(support("c", 1.0), 0)], Some(90), now);
        let three_links = core::array::from_fn::<_, 3, _>(|_| timed(support("c", 1.0), 0));
        let three_supports = chunk_link_score(&three_links, Some(90), now);
        let contradicted = chunk_link_score(
            &[
                timed(support("c", 1.0), 0),
                TimedLink {
                    link: MemoryLink {
                        from: "s".into(),
                        to: "c".into(),
                        kind: MemoryLinkKind::Contradicts,
                        weight: 1.0,
                    },
                    created_unix: now,
                },
            ],
            Some(90),
            now,
        );
        assert!(three_supports > single_support, "more support must score higher");
        assert!(single_support > contradicted, "contradiction must lower the score");
        assert!(contradicted >= 0.0, "score is clamped at the floor");
        assert!(contradicted < single_support, "contradiction never boosts the score");
    }

    #[test]
    fn score_decays_with_age() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let fresh = chunk_link_score(&[timed(support("c", 1.0), 0)], Some(90), now);
        let old = chunk_link_score(&[timed(support("c", 1.0), 60)], Some(90), now);
        let ancient = chunk_link_score(&[timed(support("c", 1.0), 95)], Some(90), now);
        assert!(fresh > old && old > 0.0 && ancient == 0.0, "evidence decays to the A5 floor");
    }

    #[test]
    fn timed_link_from_db_rejects_unparseable_created_at() {
        let link = support("c", 1.0);
        assert!(TimedLink::from_db(link.clone(), "2023-11-14 22:13:20.0 +00:00:00").is_some());
        assert!(TimedLink::from_db(link, "garbage").is_none());
    }
}
