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

/// Identity of a tracked process group, captured once at arm time.
///
/// [`Self::leader_start`] is the group leader's `starttime` (`/proc/<pid>/stat`
/// field 22, clock ticks since boot). A pid can be recycled: once the tracked
/// group has fully exited, a new process group may reuse the same pgid number.
/// Requiring the leader's start time to be unchanged across samples detects
/// that recycle and prevents attributing a foreign group's CPU to our budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessLineage {
    /// Process-group id being sampled (the leader's pid at arm time).
    pub pgid: u32,
    /// The leader's `starttime`, unchanged for the life of the group.
    pub leader_start: u64,
}

/// Capture the lineage of the process group led by `pgid`.
///
/// Returns `None` when accounting is unsupported or the leader's stat line
/// cannot be read (e.g. it already exited); callers then leave the watchdog
/// disarmed rather than sampling without a reuse check.
pub(crate) fn capture_lineage(pgid: u32) -> Option<ProcessLineage> {
    if !supported() {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pgid}/stat")).ok()?;
    let sample = parse_stat(&stat)?;
    // The leader's own line must name the group it leads.
    if sample.pgrp != pgid {
        return None;
    }
    Some(ProcessLineage { pgid, leader_start: sample.start_time })
}

/// Aggregate user+system CPU time burned so far by every live process whose
/// process-group id is `lineage.pgid`.
///
/// Returns `None` when the accounting source itself is unavailable (no
/// `/proc`), which callers treat as a *permanent* disable of the watchdog —
/// never as a zero reading — so a missing source can never be mistaken for an
/// idle group. A group that simply has no members reads `Some(0)`.
///
/// A pid whose group id matches but whose leader `starttime` does not match the
/// captured lineage signals a recycled pgid: the tracked group is gone, and the
/// reading is `Some(0)` rather than a foreign group's CPU (see
/// [`ProcessLineage`]).
///
/// The scan is per-sample and only ever runs for an opted-in budget, so the
/// default (budget off) path never pays for it.
pub(crate) fn group_cpu_time(lineage: &ProcessLineage) -> Option<Duration> {
    if !supported() {
        return None;
    }
    let hz = ticks_per_second();
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut samples = Vec::new();
    for entry in entries.flatten() {
        // `/proc` also holds non-numeric names (`self`, `net`, ...), which the
        // parse below skips; a process exiting mid-scan only fails its own read.
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some(sample) = parse_stat(&stat) {
            samples.push((pid, sample));
        }
    }
    Some(ticks_to_duration(group_ticks(&samples, lineage), hz))
}

/// One parsed `/proc/<pid>/stat` sample: the fields the CPU accounting needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatSample {
    /// Field 5: process-group id.
    pgrp: u32,
    /// Field 14: user CPU ticks.
    utime: u64,
    /// Field 15: system CPU ticks.
    stime: u64,
    /// Field 22: start time (clock ticks since boot).
    start_time: u64,
}

/// Sum the user+system ticks of every sample in `lineage`'s group, or `0` if the
/// pgid has been recycled.
///
/// Pure so the pid-reuse decision is unit-testable without `/proc`.
fn group_ticks(entries: &[(u32, StatSample)], lineage: &ProcessLineage) -> u64 {
    let mut ticks = 0u64;
    for (pid, sample) in entries {
        if sample.pgrp != lineage.pgid {
            continue;
        }
        // The leader's pid reappeared with a different start time: the pgid was
        // recycled by a foreign group, so nothing here belongs to our budget.
        if *pid == lineage.pgid && sample.start_time != lineage.leader_start {
            return 0;
        }
        ticks = ticks.saturating_add(sample.utime.saturating_add(sample.stime));
    }
    ticks
}

/// Parse the fields of a `/proc/<pid>/stat` line that CPU accounting needs, or
/// `None` when the line is malformed.
///
/// The line is `<pid> (<comm>) <state> ...`; `comm` is free-form and may itself
/// contain `)` and spaces, so the fixed-width fields are read from the *last*
/// `)` onward — `comm` is the only field before them that can contain that
/// character, so the last `)` is its terminator.
fn parse_stat(stat: &str) -> Option<StatSample> {
    let terminator = stat.rfind(')')?;
    let mut fields = stat[terminator + 1..].split_whitespace();
    let _state = fields.next()?; // field 3
    let _ppid = fields.next()?; // field 4
    let pgrp: u32 = fields.next()?.parse().ok()?; // field 5: the group we sample
                                                  // Fields 6..=13 (session, tty_nr, tpgid, flags, minflt, cminflt, majflt,
                                                  // cmajflt) sit between the group id and the CPU tick counters.
    for _ in 0..8 {
        fields.next()?;
    }
    let utime: u64 = fields.next()?.parse().ok()?; // field 14
    let stime: u64 = fields.next()?.parse().ok()?; // field 15
                                                   // Fields 16..=21 (cutime, cstime, priority, nice, num_threads,
                                                   // itrealvalue) precede the start time.
    for _ in 0..6 {
        fields.next()?;
    }
    let start_time: u64 = fields.next()?.parse().ok()?; // field 22
    Some(StatSample { pgrp, utime, stime, start_time })
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
    fn stat_line_parses_group_cpu_and_start_time() {
        // comm contains a `)`; the fixed fields start after the LAST one.
        let stat = "1234 (a comm with ) paren) S 1 1234 1234 0 -1 0 0 0 0 0 50 25 0 0 20 0 1 0 99";
        let sample = parse_stat(stat).expect("parses");
        assert_eq!(sample.pgrp, 1234);
        assert_eq!(sample.utime.saturating_add(sample.stime), 75);
        assert_eq!(sample.start_time, 99);
    }

    #[test]
    fn stat_line_of_another_group_is_not_counted() {
        let stat = "1234 (sh) S 1 999 1234 0 -1 0 0 0 0 0 50 25 0 0 20 0 1 0 99";
        let sample = parse_stat(stat).expect("parses");
        let lineage = ProcessLineage { pgid: 1234, leader_start: 99 };
        assert_eq!(group_ticks(&[(1234, sample)], &lineage), 0);
    }

    #[test]
    fn malformed_stat_line_yields_none() {
        assert!(parse_stat("1234 (sh) S").is_none());
        assert!(parse_stat("1234 (sh)").is_none());
        assert!(parse_stat("").is_none());
        // Truncated before field 22: not enough to verify lineage.
        assert!(parse_stat("1234 (sh) S 1 1234 1234 0 -1 0 0 0 0 0 50 25").is_none());
    }

    #[test]
    fn pid_reuse_does_not_attribute_foreign_cpu() {
        let tracked = ProcessLineage { pgid: 100, leader_start: 5000 };
        let own = [
            (100, StatSample { pgrp: 100, utime: 30, stime: 10, start_time: 5000 }),
            (101, StatSample { pgrp: 100, utime: 5, stime: 5, start_time: 7000 }),
        ];
        assert_eq!(group_ticks(&own, &tracked), 50, "own group is summed");

        // The same pgid number reappears with a different leader start time:
        // the original group is gone and nothing is attributed to the budget.
        let recycled = [
            (100, StatSample { pgrp: 100, utime: 999, stime: 500, start_time: 9999 }),
            (102, StatSample { pgrp: 100, utime: 100, stime: 100, start_time: 9999 }),
        ];
        assert_eq!(group_ticks(&recycled, &tracked), 0, "recycled pgid must not leak CPU");
    }

    #[test]
    fn normal_sampling_unchanged() {
        let lineage = ProcessLineage { pgid: 7, leader_start: 42 };
        let samples = [
            (7, StatSample { pgrp: 7, utime: 10, stime: 20, start_time: 42 }),
            (8, StatSample { pgrp: 7, utime: 1, stime: 2, start_time: 43 }),
            (9, StatSample { pgrp: 9, utime: 100, stime: 100, start_time: 44 }),
        ];
        assert_eq!(group_ticks(&samples, &lineage), 33);
    }

    #[test]
    fn budget_breach_still_detected() {
        let lineage = ProcessLineage { pgid: 7, leader_start: 42 };
        let limit = 500u64;
        let over = [(7, StatSample { pgrp: 7, utime: 400, stime: 200, start_time: 42 })];
        assert!(group_ticks(&over, &lineage) >= limit, "a real breach must still be visible");
        let recycled = [(7, StatSample { pgrp: 7, utime: 400, stime: 200, start_time: 4242 })];
        assert!(
            group_ticks(&recycled, &lineage) < limit,
            "a recycled pgid must not fire the budget"
        );
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
        let lineage = ProcessLineage { pgid: 0x7FFF_FFFE, leader_start: 0 };
        assert_eq!(group_cpu_time(&lineage), Some(Duration::ZERO));
    }
}
