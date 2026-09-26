//! Process-group CPU-time accounting for the shell CPU budget
//! (threat gap #6, `docs/security-threat-model.md` §6).
//!
//! Samples the aggregate user+system CPU time of a whole process group so a
//! configured budget still bites when the kernel's per-process `RLIMIT_CPU`
//! backstop cannot (argv-direct plans) or does not (hosts without `/proc`).
//! Sampling is Linux-only: the per-process tick counters live in
//! `/proc/<pid>/stat`, which no other supported host mounts.

use std::time::Duration;

/// How often [`group_cpu_time`] is polled by the spawn watchdog.
///
/// Short enough that a budget is overshot by at most one interval, long
/// enough that a 5-minute ceiling costs at most 1200 samples.
pub(crate) const CPU_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

/// True when process-group CPU time can be sampled on this host.
///
/// Only Linux exposes `/proc/<pid>/stat`. Elsewhere callers fail open (a
/// `ulimit` backstop applied to a POSIX-wrapped shell plan still bounds the
/// command) and warn, rather than claiming an enforcement they cannot perform.
pub(crate) fn supported() -> bool {
    cfg!(target_os = "linux")
}

/// Aggregate user+system CPU time burned so far by every live process whose
/// process-group id is `pgid`.
///
/// Returns `None` when the accounting source itself is unavailable (no
/// `/proc`), which callers treat as a *permanent* disable of the watchdog —
/// never as a zero reading — so a missing source can never be mistaken for an
/// idle group. A group that simply has no members reads `Some(0)`.
///
/// The scan is per-sample and only ever runs for an opted-in budget, so the
/// default (budget off) path never pays for it.
pub(crate) fn group_cpu_time(pgid: u32) -> Option<Duration> {
    if !supported() {
        return None;
    }
    let hz = ticks_per_second();
    let mut ticks: u64 = 0;
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        // `/proc` also holds non-numeric names (`self`, `net`, ...), which the
        // parse below skips; a process exiting mid-scan only fails its own read.
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some(group_ticks) = stat_group_ticks(&stat, pgid) {
            ticks = ticks.saturating_add(group_ticks);
        }
    }
    Some(ticks_to_duration(ticks, hz))
}

/// `(pgrp, utime + stime)` from a `/proc/<pid>/stat` line, or `None` when the
/// line is malformed or belongs to another process group.
///
/// The line is `<pid> (<comm>) <state> ...`; `comm` is free-form and may
/// itself contain `)` and spaces, so the fixed-width fields are read from the
/// *last* `)` onward — `comm` is the only field before them that can contain
/// that character, so the last `)` is its terminator.
fn stat_group_ticks(stat: &str, pgid: u32) -> Option<u64> {
    let terminator = stat.rfind(')')?;
    let mut fields = stat[terminator + 1..].split_whitespace();
    let _state = fields.next()?; // field 3
    let _ppid = fields.next()?; // field 4
    let pgrp: u32 = fields.next()?.parse().ok()?; // field 5: the group we sample
    if pgrp != pgid {
        return None;
    }
    // Fields 6..=13 (session, tty_nr, tpgid, flags, minflt, cminflt, majflt,
    // cmajflt) sit between the group id and the CPU tick counters.
    for _ in 0..8 {
        fields.next()?;
    }
    let utime: u64 = fields.next()?.parse().ok()?; // field 14
    let stime: u64 = fields.next()?.parse().ok()?; // field 15
    Some(utime.saturating_add(stime))
}

/// Convert a jiffy count into wall units using `CLK_TCK`.
///
/// `hz` is clamped to at least 1 so a degenerate reading can never divide by
/// zero; the fractional term saturates instead of overflowing.
fn ticks_to_duration(ticks: u64, hz: u64) -> Duration {
    let hz = hz.max(1);
    // `rem < hz` keeps the sub-second term below 1e9, but clamp anyway rather
    // than rely on that invariant for a conversion that would otherwise trap.
    let nanos = (ticks % hz).saturating_mul(1_000_000_000) / hz;
    Duration::new(ticks / hz, nanos.min(u64::from(u32::MAX)) as u32)
}

/// `CLK_TCK` (jiffies per second): a boot-time constant, so read it once.
///
/// Unavailable where `sysconf` is (non-Unix); the fallback matches the
/// historical Linux default and only affects the sampling granularity.
#[cfg(unix)]
fn ticks_per_second() -> u64 {
    static HZ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| {
        nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
            .ok()
            .flatten()
            .filter(|hz| *hz > 0)
            .map(|hz| hz as u64)
            .unwrap_or(100)
    })
}

#[cfg(not(unix))]
fn ticks_per_second() -> u64 {
    100
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn stat_line_yields_group_cpu_ticks() {
        // comm contains a `)`; the fixed fields start after the LAST one.
        let stat = "1234 (a comm with ) paren) S 1 1234 1234 0 -1 0 0 0 0 0 50 25 0 0 20 0 1 0 99";
        assert_eq!(stat_group_ticks(stat, 1234), Some(75));
    }

    #[test]
    fn stat_line_of_another_group_is_not_counted() {
        let stat = "1234 (sh) S 1 999 1234 0 -1 0 0 0 0 0 50 25";
        assert_eq!(stat_group_ticks(stat, 1234), None);
    }

    #[test]
    fn malformed_stat_line_yields_none() {
        assert_eq!(stat_group_ticks("1234 (sh) S", 1234), None);
        assert_eq!(stat_group_ticks("1234 (sh)", 1234), None);
        assert_eq!(stat_group_ticks("", 1234), None);
    }

    #[test]
    fn ticks_convert_with_clk_tck() {
        assert_eq!(ticks_to_duration(150, 100), Duration::from_millis(1500));
        assert_eq!(ticks_to_duration(0, 100), Duration::ZERO);
        // A degenerate clock never panics: it is clamped, not divided by zero.
        assert_eq!(ticks_to_duration(5, 0), Duration::from_secs(5));
    }

    #[test]
    fn accounting_is_supported_here() {
        assert!(supported());
        assert!(ticks_per_second() > 0);
    }

    #[test]
    fn empty_group_reads_zero_not_unsupported() {
        // Above the default pid_max, so it cannot name a real process group.
        assert_eq!(group_cpu_time(0x7FFF_FFFE), Some(Duration::ZERO));
    }
}
