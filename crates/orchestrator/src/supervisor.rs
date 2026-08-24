//! Supervisor process-lifecycle core — ADR-60 vertical slice S4a.
//!
//! This module holds the heart of the supervisor (ADR-60 D1): configuration
//! knobs ([`SupervisorConfig`]), the per-agent lifecycle state machine
//! ([`AgentState`]), liveness bookkeeping ([`AgentMeta`]), the one_for_one
//! restart policy ([`should_restart`], [`restart_backoff`], [`jitter`]), and
//! the process lifecycle: spawning an agent child, the versioned stdio
//! handshake (ADR-60 D2), graceful teardown via stdin-close → grace →
//! SIGKILL, one-for-one restart attempts from a snapshotted respawn
//! spec, and the steady-state message loop (heartbeat handling, stale
//! detection, driving restarts from events). With write-path services
//! attached ([`SupervisorServices`]), the loop dispatches `execute-tool` /
//! `publish-event` / `retrieve-memory` requests to the async task pool:
//! policy evaluation and tool execution happen in the single write gate
//! (ADR-60 D4), events are appended to the whiteboard log (D3), and memory
//! queries resolve against the shared memory spine (D6).
//!
//! Concurrency structure (deliberately simple): each agent gets a dedicated
//! blocking reader thread that drains the child's stdout into a `mpsc` channel
//! of line events ([`LineEvent`], mirroring the framing contract in
//! `crate::ipc`). Writes are serialized on the child's stdin with
//! `ipc::serialize_frame`. [`Supervisor::spawn_agent`] and
//! [`Supervisor::stop_agent`] are synchronous (they block on the handshake /
//! grace window); [`Supervisor::restart_agent`] is async (backoff sleep);
//! the async message loop that feeds on [`LineEvent`]s is the remaining
//! chunk.
//!
//! The orphan model per ADR-60: each agent child sets `PR_SET_PDEATHSIG` on
//! itself so it dies with its supervisor (deferred to the agent-process entry,
//! S5), and the supervisor's own cleanup path is stdin-close → child
//! exits → reap.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::gate::{stamp_base_versions, GateError, GateRequest, WriteGate};
use crate::ipc::{
    self, IpcError, IpcErrorCode, IpcMethod, IpcNotification, IpcParams, IpcRequest, IpcResponse,
    IpcResult, IpcTransportError, MAX_MESSAGE_BYTES,
};
use crate::subscriptions::SubscriptionManager;
use concerto_core::memory::{MemoryNamespace, MemoryQuery, ProjectId};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::CancellationToken;
use concerto_sessions::whiteboard::{append_whiteboard_event, NewWhiteboardEvent, WhiteboardScope};

/// Cap for the exponential restart backoff ([`restart_backoff`] clip).
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(60);

/// Tunables for one supervisor instance (ADR-60 D1).
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorConfig {
    /// Time a freshly spawned agent has to complete the versioned handshake
    /// (ADR-60 D2) before the supervisor aborts the spawn.
    pub handshake_timeout: Duration,
    /// Liveness window: an agent that emits no heartbeat within this interval
    /// is presumed dead (see [`AgentMeta::is_stale`]).
    pub heartbeat_timeout: Duration,
    /// One_for_one intensity cap: maximum restarts per agent before the
    /// supervisor gives up on it.
    pub max_restarts: u32,
    /// Base exponential backoff delay before the first restart.
    pub restart_backoff: Duration,
    /// Fraction of the backoff added as jitter, `0.0..=1.0`.
    pub restart_backoff_jitter: f64,
    /// Grace period between requesting a stop (stdin-close) and escalating to
    /// SIGKILL (ADR-60 D1).
    pub kill_grace: Duration,
    /// ADR-60 D3 whiteboard subscription push: agent id → topic scopes whose
    /// events the supervisor streams to that agent as `whiteboard-slice`
    /// notifications (protocol 0.2.0). Defaults to empty (no pushes).
    pub whiteboard_subscriptions: std::collections::HashMap<String, Vec<WhiteboardScope>>,
}

impl SupervisorConfig {
    /// Declare that `agent_id` receives a whiteboard subscription push for
    /// `topics` (ADR-60 D3; scopes are config-owned at spawn, ADR-58/59).
    pub fn with_whiteboard_subscription(
        mut self,
        agent_id: impl Into<String>,
        topics: Vec<concerto_sessions::whiteboard::WhiteboardKind>,
    ) -> Self {
        self.whiteboard_subscriptions.insert(agent_id.into(), vec![WhiteboardScope { topics }]);
        self
    }
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            heartbeat_timeout: Duration::from_secs(30),
            max_restarts: 3,
            restart_backoff: Duration::from_millis(500),
            restart_backoff_jitter: 0.3,
            kill_grace: Duration::from_secs(2),
            whiteboard_subscriptions: std::collections::HashMap::new(),
        }
    }
}

/// Lifecycle state machine for one agent process (ADR-60 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Spawned but not yet through the versioned handshake (ADR-60 D2).
    Starting,
    /// Handshake complete; liveness heartbeats are flowing.
    Running,
    /// Stop requested (stdin closed); waiting out the kill grace period.
    Stopping,
    /// Stopped cleanly and reaped.
    Stopped,
    /// Unrecoverable failure (restart cap exhausted or fatal error).
    Failed,
    /// The task completed: the child exited `0` on its own (ADR-60 S5
    /// one-run-per-process). Terminal — never restarted.
    Completed,
}

/// Per-agent liveness bookkeeping for the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMeta {
    /// Stable identity of the agent this metadata tracks.
    pub agent_id: String,
    /// Current lifecycle state.
    pub state: AgentState,
    /// Wall-clock (Unix epoch, milliseconds) of the last accepted heartbeat;
    /// `0` means never heard from.
    pub last_seen_ms: i64,
    /// Highest heartbeat sequence accepted; monotonic per agent.
    pub seq: u64,
    /// Number of restarts this agent has already consumed.
    pub restart_count: u32,
    /// Wall-clock (Unix epoch, milliseconds) the agent entered [`AgentState::Failed`].
    pub failed_at_ms: Option<i64>,
}

impl AgentMeta {
    /// Metadata for a freshly spawned agent in [`AgentState::Starting`] with
    /// no recorded heartbeat (`last_seen_ms == 0`), sequence `0`, and zero
    /// restarts.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            state: AgentState::Starting,
            last_seen_ms: 0,
            seq: 0,
            restart_count: 0,
            failed_at_ms: None,
        }
    }

    /// Record one liveness heartbeat, enforcing:
    ///
    /// - `seq` must be strictly greater than the current sequence — otherwise
    ///   [`HeartbeatError::StaleSeq`] with `expected = seq + 1`. Gaps are
    ///   allowed (the supervisor detects, not rejects, them); replays and
    ///   older sequences are rejected.
    /// - `now_ms` must not be earlier than `last_seen_ms` — otherwise
    ///   [`HeartbeatError::ClockWentBackwards`]. Equal timestamps are legal.
    ///
    /// On success `last_seen_ms` and `seq` are updated; on failure the
    /// metadata is left untouched. `hb_timeout_ms` is reserved for a
    /// stale-agent rejection in a later chunk — staleness is only observed
    /// through [`AgentMeta::is_stale`] today.
    pub fn record_heartbeat(
        &mut self,
        seq: u64,
        now_ms: i64,
        hb_timeout_ms: i64,
    ) -> Result<(), HeartbeatError> {
        let _ = hb_timeout_ms;
        if seq <= self.seq {
            return Err(HeartbeatError::StaleSeq {
                expected: self.seq.saturating_add(1),
                got: seq,
            });
        }
        if now_ms < self.last_seen_ms {
            return Err(HeartbeatError::ClockWentBackwards {
                last: self.last_seen_ms,
                now: now_ms,
            });
        }
        self.seq = seq;
        self.last_seen_ms = now_ms;
        Ok(())
    }

    /// Whether the agent is presumed dead: `now_ms - last_seen_ms` strictly
    /// exceeds `hb_timeout_ms`. Exactly-at-timeout is not stale, and a query
    /// with `now_ms` earlier than `last_seen_ms` (clock skew) saturates to
    /// "not stale". A fresh [`AgentMeta`] (never heard from) is stale once
    /// `now_ms` passes the timeout.
    pub fn is_stale(&self, now_ms: i64, hb_timeout_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) > hb_timeout_ms
    }
}

/// Why a heartbeat was rejected by [`AgentMeta::record_heartbeat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatError {
    /// `got` is not strictly greater than the last accepted sequence; the
    /// next acceptable value was `expected`.
    StaleSeq { expected: u64, got: u64 },
    /// `now` is earlier than the last accepted timestamp `last` — the clock
    /// moved backwards between heartbeats.
    ClockWentBackwards { last: i64, now: i64 },
}

impl std::fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleSeq { expected, got } => {
                write!(f, "stale heartbeat sequence: expected seq {expected}, got {got}")
            }
            Self::ClockWentBackwards { last, now } => {
                write!(f, "heartbeat clock went backwards: last seen {last}ms, now {now}ms")
            }
        }
    }
}

impl std::error::Error for HeartbeatError {}

/// One_for_one intensity gate: `true` while `restart_count < max_restarts`
/// (the next restart is permitted); `false` once the count reaches the cap.
pub fn should_restart(restart_count: u32, max_restarts: u32) -> bool {
    restart_count < max_restarts
}

/// Exponential backoff for restart `restart_count`: `base * 2^restart_count`,
/// clipped to `max_backoff`. All math is saturating, so no overflow is
/// possible.
pub fn restart_backoff(base: Duration, restart_count: u32, max_backoff: Duration) -> Duration {
    base.saturating_mul(2u32.saturating_pow(restart_count)).min(max_backoff)
}

/// Deterministic placeholder jitter for a restart delay (ADR-60 D1 backoff +
/// jitter). Even restart counts scale the backoff by `1 + 0.25·ratio`, odd
/// counts by `1 + 0.75·ratio`, keeping the result within
/// `[backoff, backoff + backoff·ratio]` and below `2×` the input. This is a
/// stand-in until real randomness is introduced in a later chunk: it stays
/// deterministic for tests while preserving the noise-band shape.
pub fn jitter(backoff: Duration, restart_count: u32, jitter_ratio: f64) -> Duration {
    let factor = jitter_ratio * if restart_count.is_multiple_of(2) { 0.25 } else { 0.75 };
    Duration::from_secs_f64(backoff.as_secs_f64() * (1.0 + factor))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time proof that the error implements `std::error::Error`.
    fn is_std_error<E: std::error::Error>(_error: &E) {}

    #[test]
    fn supervisor_config_defaults() {
        let config = SupervisorConfig::default();
        assert_eq!(config.handshake_timeout, Duration::from_secs(10));
        assert_eq!(config.heartbeat_timeout, Duration::from_secs(30));
        assert_eq!(config.max_restarts, 3);
        assert_eq!(config.restart_backoff, Duration::from_millis(500));
        assert_eq!(config.restart_backoff_jitter, 0.3);
        assert_eq!(config.kill_grace, Duration::from_secs(2));
    }

    #[test]
    fn agent_meta_new_defaults() {
        let meta = AgentMeta::new("agent-a");
        assert_eq!(meta.agent_id, "agent-a");
        assert_eq!(meta.state, AgentState::Starting);
        assert_eq!(meta.last_seen_ms, 0);
        assert_eq!(meta.seq, 0);
        assert_eq!(meta.restart_count, 0);
        assert!(meta.failed_at_ms.is_none());
        // `Into<String>` also accepts owned strings.
        let owned = AgentMeta::new(String::from("agent-b"));
        assert_eq!(owned.agent_id, "agent-b");
    }

    #[test]
    fn record_heartbeat_happy_path() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(1, 1_000, 30_000).expect("first heartbeat accepted");
        assert_eq!(meta.seq, 1);
        assert_eq!(meta.last_seen_ms, 1_000);
        // Only liveness fields move; lifecycle bookkeeping is untouched.
        assert_eq!(meta.state, AgentState::Starting);
        assert_eq!(meta.restart_count, 0);
        assert!(meta.failed_at_ms.is_none());
    }

    #[test]
    fn record_heartbeat_rejects_stale_seq_with_exact_fields() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(1, 1_000, 30_000).expect("first heartbeat accepted");
        let error =
            meta.record_heartbeat(1, 2_000, 30_000).expect_err("replayed seq must be rejected");
        assert_eq!(error, HeartbeatError::StaleSeq { expected: 2, got: 1 });
        is_std_error(&error);
        let message = error.to_string();
        assert!(message.contains("expected seq 2"), "display {message:?}");
        assert!(message.contains("got 1"), "display {message:?}");
    }

    #[test]
    fn record_heartbeat_rejects_seq_again_and_older_after_success() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(5, 10_000, 30_000).expect("accepted");
        // The same seq again, and any older seq, are both stale.
        let error = meta.record_heartbeat(5, 20_000, 30_000).expect_err("replay rejected");
        assert_eq!(error, HeartbeatError::StaleSeq { expected: 6, got: 5 });
        let error = meta.record_heartbeat(3, 30_000, 30_000).expect_err("older seq rejected");
        assert_eq!(error, HeartbeatError::StaleSeq { expected: 6, got: 3 });
        // Rejected heartbeats must not mutate the metadata.
        assert_eq!(meta.last_seen_ms, 10_000);
        assert_eq!(meta.seq, 5);
    }

    #[test]
    fn record_heartbeat_allows_seq_gaps() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(1, 1_000, 30_000).expect("accepted");
        // Monotonicity is enforced, contiguity is not: the supervisor uses
        // `seq` to *detect* gaps (ADR-60 D1) rather than reject them.
        meta.record_heartbeat(4, 2_000, 30_000).expect("gap accepted");
        assert_eq!(meta.seq, 4);
    }

    #[test]
    fn record_heartbeat_accepts_equal_timestamp_with_higher_seq() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(1, 2_000, 30_000).expect("accepted");
        // Same-ms heartbeats are legitimate; only strictly-earlier times are
        // a clock violation.
        meta.record_heartbeat(2, 2_000, 30_000).expect("equal timestamp accepted");
        assert_eq!(meta.last_seen_ms, 2_000);
        assert_eq!(meta.seq, 2);
    }

    #[test]
    fn record_heartbeat_rejects_clock_backwards() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(1, 2_000, 30_000).expect("accepted");
        let error =
            meta.record_heartbeat(2, 1_500, 30_000).expect_err("clock going backwards rejected");
        assert_eq!(error, HeartbeatError::ClockWentBackwards { last: 2_000, now: 1_500 });
        is_std_error(&error);
        assert!(error.to_string().contains("last seen 2000ms"), "display {:?}", error.to_string());
        // Failure leaves the metadata untouched.
        assert_eq!(meta.last_seen_ms, 2_000);
        assert_eq!(meta.seq, 1);
    }

    #[test]
    fn is_stale_boundary_at_timeout_is_not_stale() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(1, 1_000, 30_000).expect("accepted");
        // Exactly at the timeout the agent is still presumed alive...
        assert!(!meta.is_stale(1_000 + 30_000, 30_000));
        // ...and one millisecond later it is not.
        assert!(meta.is_stale(1_000 + 30_000 + 1, 30_000));
    }

    #[test]
    fn is_stale_resets_after_record_heartbeat() {
        // A fresh agent has never been heard from (`last_seen_ms == 0`).
        let mut meta = AgentMeta::new("agent-a");
        assert!(!meta.is_stale(30_000, 30_000));
        assert!(meta.is_stale(30_001, 30_000));
        meta.record_heartbeat(1, 1_000, 30_000).expect("accepted");
        // The new last_seen resets the liveness window.
        assert!(!meta.is_stale(1_000, 30_000));
        assert!(!meta.is_stale(31_000, 30_000));
        assert!(meta.is_stale(31_001, 30_000));
    }

    #[test]
    fn should_restart_true_below_max() {
        // max_restarts = 3 → restarts 0, 1, 2 are allowed; the 4th attempt
        // (count 3) exceeds the intensity cap.
        for count in 0..3 {
            assert!(should_restart(count, 3), "count {count} under cap must restart");
        }
    }

    #[test]
    fn should_restart_false_at_or_above_max() {
        assert!(!should_restart(3, 3));
        assert!(!should_restart(4, 3));
        // A zero cap permits no restarts at all.
        assert!(!should_restart(0, 0));
    }

    #[test]
    fn restart_backoff_doubles_until_capped() {
        let base = Duration::from_millis(500);
        let max = Duration::from_secs(60);
        let cases = [
            (0u32, Duration::from_millis(500)),
            (1, Duration::from_millis(1_000)),
            (2, Duration::from_millis(2_000)),
            (3, Duration::from_millis(4_000)),
            (4, Duration::from_millis(8_000)),
            (5, Duration::from_millis(16_000)),
            (6, Duration::from_millis(32_000)),
            (7, Duration::from_millis(60_000)),
            (8, Duration::from_millis(60_000)),
            (9, Duration::from_millis(60_000)),
            (10, Duration::from_millis(60_000)),
        ];
        for (count, expected) in cases {
            assert_eq!(restart_backoff(base, count, max), expected, "count {count}");
        }
    }

    #[test]
    fn restart_backoff_clips_base_above_max() {
        // The clip applies to the computed delay, so a base already over the
        // cap — and a huge restart count — saturate instead of overflowing.
        assert_eq!(
            restart_backoff(Duration::from_secs(120), 0, Duration::from_secs(60)),
            Duration::from_secs(60)
        );
        assert_eq!(
            restart_backoff(Duration::from_millis(1), u32::MAX, Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    const JITTER_EPS_SECS: f64 = 1e-9;

    #[test]
    fn jitter_stays_within_ratio_band() {
        let base = Duration::from_millis(500);
        let ratio = 0.3;
        let lo = base.as_secs_f64();
        let hi = base.as_secs_f64() * (1.0 + ratio);
        for count in 0..8 {
            let jittered = jitter(base, count, ratio).as_secs_f64();
            assert!(
                jittered >= lo - JITTER_EPS_SECS && jittered <= hi + JITTER_EPS_SECS,
                "count {count}: {jittered}s outside [{lo}, {hi}]s"
            );
        }
    }

    #[test]
    fn jitter_alternates_by_parity_and_is_deterministic() {
        let base = Duration::from_millis(500);
        let ratio = 0.3;
        // Odd restarts jitter more (+75% of the ratio) than even (+25%).
        assert!(jitter(base, 1, ratio) > jitter(base, 0, ratio), "odd must jitter more than even");
        // Deterministic: same inputs → same output, and equal parity →
        // equal output regardless of the absolute count.
        assert_eq!(jitter(base, 0, ratio), jitter(base, 0, ratio));
        assert_eq!(jitter(base, 2, ratio), jitter(base, 0, ratio));
        assert_eq!(jitter(base, 3, ratio), jitter(base, 1, ratio));
    }

    #[test]
    fn jitter_zero_ratio_is_exact_backoff() {
        // A zero ratio must not change the delay at all.
        assert_eq!(jitter(Duration::from_millis(500), 0, 0.0), Duration::from_millis(500));
        assert_eq!(jitter(Duration::from_millis(500), 1, 0.0), Duration::from_millis(500));
    }
}

// ===========================================================================
// Process lifecycle (S4a-iii): spawn, versioned handshake, graceful teardown.
// ===========================================================================

/// Why an agent lifecycle operation failed.
#[derive(Debug)]
pub enum SupervisorError {
    /// The child could not be spawned.
    Spawn(std::io::Error),
    /// The child never completed the handshake within [`SupervisorConfig::handshake_timeout`];
    /// it was killed and reaped.
    HandshakeTimeout { agent_id: String, waited: Duration },
    /// The child answered the handshake but was refused (version mismatch or
    /// `accepted: false`); it was killed and reaped.
    HandshakeRejected { agent_id: String, reason: String },
    /// The child exited (or closed stdout) before completing the handshake.
    ChildExited { agent_id: String, status: String },
    /// A pipe I/O failure while driving the child.
    Io(std::io::Error),
    /// A protocol violation: malformed frame, unparseable reply, or a reply
    /// with the wrong shape.
    Protocol(String),
    /// No agent with this id is registered.
    UnknownAgent(String),
    /// An agent with this id is already registered.
    AlreadyRunning(String),
    /// The one_for_one restart cap ([`SupervisorConfig::max_restarts`]) was
    /// reached; the agent is left in place for the caller to mark failed.
    RestartsExhausted { agent_id: String, restart_count: u32 },
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "failed to spawn agent child: {error}"),
            Self::HandshakeTimeout { agent_id, waited } => {
                write!(f, "agent {agent_id} did not complete the handshake within {waited:?}")
            }
            Self::HandshakeRejected { agent_id, reason } => {
                write!(f, "agent {agent_id} handshake rejected: {reason}")
            }
            Self::ChildExited { agent_id, status } => {
                write!(f, "agent {agent_id} exited during handshake: {status}")
            }
            Self::Io(error) => write!(f, "agent I/O error: {error}"),
            Self::Protocol(message) => write!(f, "agent protocol error: {message}"),
            Self::UnknownAgent(agent_id) => write!(f, "no agent registered as {agent_id}"),
            Self::AlreadyRunning(agent_id) => write!(f, "agent {agent_id} is already registered"),
            Self::RestartsExhausted { agent_id, restart_count } => {
                write!(f, "agent {agent_id} exhausted its restart budget (max {restart_count})")
            }
        }
    }
}

impl std::error::Error for SupervisorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<IpcTransportError> for SupervisorError {
    fn from(error: IpcTransportError) -> Self {
        match error {
            IpcTransportError::Io(error) => Self::Io(error),
            other => Self::Protocol(other.to_string()),
        }
    }
}

/// One line-level event from an agent's stdout reader thread, mirroring the
/// framing contract of `crate::ipc::read_message`.
#[derive(Debug)]
pub enum LineEvent {
    /// One complete, UTF-8, non-empty line; terminator removed, a trailing
    /// `\r` (CRLF) stripped.
    Message(String),
    /// Clean EOF at a line boundary: the child closed its stdout.
    Eof,
    /// A framing violation surfaced by the reader thread.
    Error(IpcTransportError),
}

/// Drain `reader` until EOF, emitting one [`LineEvent`] per line. Runs on a
/// dedicated per-agent thread (std blocking I/O). Semantics mirror
/// `crate::ipc::read_message`: `\n` terminates a message, a trailing `\r` is
/// tolerated, the final unterminated line at EOF is still delivered, and
/// empty / over-long / non-UTF-8 lines surface as [`LineEvent::Error`] so the
/// caller (not the reader) decides policy.
fn reader_events<R: BufRead>(mut reader: R, tx: Sender<LineEvent>, max_len: usize) {
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(error) => {
                let _ = tx.send(LineEvent::Error(IpcTransportError::Io(error)));
                return;
            }
        };
        if n == 0 {
            // Clean EOF at a line boundary.
            let _ = tx.send(LineEvent::Eof);
            return;
        }
        if line.last() != Some(&b'\n') {
            // Final unterminated line of the stream: deliver it, then let
            // the next read report the EOF.
            emit_line(&tx, std::mem::take(&mut line), max_len);
            continue;
        }
        line.pop(); // '\n'
        emit_line(&tx, std::mem::take(&mut line), max_len);
    }
}

/// Validate one line (terminator already removed) and ship it as a
/// [`LineEvent`].
fn emit_line(tx: &Sender<LineEvent>, mut line: Vec<u8>, max_len: usize) {
    // Tolerate a trailing `\r` (CRLF); `\n` is the only terminator.
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.len() > max_len {
        let _ =
            tx.send(LineEvent::Error(IpcTransportError::Oversized { len: line.len(), max_len }));
        return;
    }
    if line.is_empty() {
        let _ = tx.send(LineEvent::Error(IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty line is not a valid IPC message",
        ))));
        return;
    }
    match String::from_utf8(line) {
        Ok(text) => {
            let _ = tx.send(LineEvent::Message(text));
        }
        Err(_) => {
            let _ = tx.send(LineEvent::Error(IpcTransportError::InvalidUtf8));
        }
    }
}

/// A respawnable spawn specification (ADR-60 D1 one_for_one).
///
/// `std::process::Command` is not `Clone`-able and exposes no environment
/// mutation, so the supervisor snapshots program, args and env at first
/// spawn and rebuilds a fresh [`Command`] per spawn attempt — first spawn
/// and every respawn see the same configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnSpec {
    program: OsString,
    args: Vec<OsString>,
    /// `(name, value)` pairs; `None` value = the variable is explicitly
    /// removed from the child environment.
    envs: Vec<(OsString, Option<OsString>)>,
}

impl RespawnSpec {
    /// Snapshot a command's program, argument list and environment.
    pub fn from_command(command: &Command) -> Self {
        Self {
            program: command.get_program().to_os_string(),
            args: command.get_args().map(ToOwned::to_owned).collect(),
            envs: command
                .get_envs()
                .map(|(name, value)| (name.to_os_string(), value.map(ToOwned::to_owned)))
                .collect(),
        }
    }

    /// A fresh [`Command`] for this spec; pipes are attached by the caller.
    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        for (name, value) in &self.envs {
            match value {
                Some(value) => {
                    command.env(name, value);
                }
                None => {
                    command.env_remove(name);
                }
            }
        }
        command
    }
}

/// One supervised agent child process: its pipes, liveness metadata, and the
/// respawn spec for one-for-one restarts.
pub struct AgentProcess {
    /// Lifecycle + liveness bookkeeping.
    pub meta: AgentMeta,
    child: Child,
    stdin: std::process::ChildStdin,
    /// Reader thread + line-event channel draining the child's stdout.
    #[allow(dead_code)]
    reader: ReaderHandle,
    /// Spawn spec used to replace this child on restart.
    respawn: RespawnSpec,
    /// Reply channel from async dispatch tasks (gate/whiteboard/memory
    /// handlers) back to the loop's writer; drained by [`Supervisor::run`]
    /// each tick.
    reply_rx: Receiver<IpcResponse>,
    /// Send end handed to spawned dispatch tasks.
    reply_tx: Sender<IpcResponse>,
    /// Set once the agent's whiteboard subscription (ADR-60 D3) has been
    /// (re-)registered with the [`SubscriptionManager`] on the run loop;
    /// a restarted child resets this via a fresh `AgentProcess`.
    subscription_registered: bool,
}

struct ReaderHandle {
    _thread: JoinHandle<()>,
    // Consumed by the steady-state message loop ([`Supervisor::run`]); held
    // here so the channel stays alive and the reader thread stays
    // owned by the agent process.
    rx: Receiver<LineEvent>,
}

/// Outcome of a [`Supervisor::run`] session: which agents gave up while the
/// loop was driving them, plus the full metadata snapshot captured at the
/// moment shutdown fired (before the graceful-stop pass). Healthy entries
/// carry their last loop state; failed entries remain registered afterwards.
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// Agents that failed during the run (restart budget exhausted or an
    /// unrecoverable spawn/io failure).
    pub failed: Vec<String>,
    /// Metadata snapshot of every registration at shutdown time.
    pub agents: Vec<AgentMeta>,
}

/// Shared write-path services the steady-state loop dispatches into
/// (ADR-60 D3/D4/D6): the single write gate, the whiteboard append pool, and
/// the memory spine. `Clone` so async dispatch tasks can carry their own
/// handles (all members are `Arc`/pool handles).
#[derive(Clone)]
pub struct SupervisorServices {
    /// The one write gate: every agent write is policy-evaluated, sequenced
    /// (whiteboard `gate_seq`) and executed here (ADR-60 D4).
    pub gate: std::sync::Arc<WriteGate>,
    /// Whiteboard log pool; `append_whiteboard_event` assigns `gate_seq`.
    pub whiteboard_pool: sqlx::SqlitePool,
    /// Memory spine used by `retrieve-memory` (ADR-60 D6).
    pub memory: std::sync::Arc<dyn MemoryStore>,
    /// Project stamped into memory queries.
    pub project_id: ProjectId,
    /// ADR-60 D3 subscription manager: per-agent cursors and bounded slices
    /// for the `whiteboard-slice` / `ack-whiteboard` push surface.
    pub subscriptions: SubscriptionManager,
    /// ADR-60 D6 consolidation projection task; `None` when no projection
    /// store is available (fail-soft degradation, mirroring the gate without
    /// a session DB). When attached, write-path handlers feed it append
    /// counts so it can detach an out-of-band fold pass onto the runtime.
    pub consolidation: Option<std::sync::Arc<crate::consolidation::Consolidator>>,
}

/// One supervisor instance: owns all agent children, their pipes, and their
/// lifecycle metadata (ADR-60 D1).
pub struct Supervisor {
    config: SupervisorConfig,
    agents: std::collections::HashMap<String, AgentProcess>,
    /// Write-path services (gate/whiteboard/memory); `None` until attached
    /// via [`Supervisor::with_services`].
    services: Option<SupervisorServices>,
}

impl Supervisor {
    /// A supervisor with the given configuration and no agents.
    pub fn new(config: SupervisorConfig) -> Self {
        Self { config, agents: std::collections::HashMap::new(), services: None }
    }

    /// Attach the write-path services (gate, whiteboard pool, memory spine).
    /// Until this is called, `execute-tool` / `publish-event` /
    /// `retrieve-memory` requests are answered `Internal` "not configured".
    pub fn with_services(mut self, services: SupervisorServices) -> Self {
        self.services = Some(services);
        self
    }

    /// Lifecycle metadata of the registered agent, if any.
    pub fn agent(&self, agent_id: &str) -> Option<&AgentMeta> {
        self.agents.get(agent_id).map(|process| &process.meta)
    }

    /// OS pid of the registered agent's live child process, if any
    /// (crash-injection / ops monitoring).
    pub fn agent_pid(&self, agent_id: &str) -> Option<u32> {
        self.agents.get(agent_id).map(|process| process.child.id())
    }

    /// Spawn `command` as agent `agent_id` and drive it through the versioned
    /// stdio handshake (ADR-60 D2).
    ///
    /// On success the agent is [`AgentState::Running`]. On any failure the
    /// child is killed (if still alive) and reaped before returning, and no
    /// agent is left registered.
    pub fn spawn_agent(
        &mut self,
        command: &mut Command,
        agent_id: &str,
    ) -> Result<(), SupervisorError> {
        if self.agents.contains_key(agent_id) {
            return Err(SupervisorError::AlreadyRunning(agent_id.to_owned()));
        }
        let spec = RespawnSpec::from_command(command);
        self.spawn_inner(&spec, agent_id, 0)
    }

    /// Spawn `spec` as agent `agent_id` with restart bookkeeping
    /// `restart_count`, driving the versioned stdio handshake (ADR-60 D2).
    ///
    /// Shared by [`Supervisor::spawn_agent`] and the one-for-one restart
    /// path. On success the agent is [`AgentState::Running`]. On any failure
    /// the child is killed (if still alive) and reaped before returning, and
    /// no agent is left registered.
    fn spawn_inner(
        &mut self,
        spec: &RespawnSpec,
        agent_id: &str,
        restart_count: u32,
    ) -> Result<(), SupervisorError> {
        let mut command = spec.to_command();
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        // Child stderr is normally discarded; `CONCERTO_SUPERVISOR_DEBUG_STDERR`
        // redirects it to a file for diagnosing agent-process failures (used
        // by tests/CI). Not a security boundary — the env is local.
        command.stderr(match std::env::var("CONCERTO_SUPERVISOR_DEBUG_STDERR") {
            Ok(path) => {
                let file =
                    std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(
                        |error| {
                            SupervisorError::Spawn(std::io::Error::other(format!(
                                "cannot open CONCERTO_SUPERVISOR_DEBUG_STDERR '{path}': {error}"
                            )))
                        },
                    )?;
                Stdio::from(file)
            }
            Err(_) => Stdio::null(),
        });
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;

        // Both pipes must exist because we just asked for them; take them out
        // of the child so we own the ends. On any misconfiguration, clean up
        // the child before bailing.
        let (mut stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => (stdin, stdout),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SupervisorError::Protocol(
                    "child was not spawned with stdin/stdout pipes".to_owned(),
                ));
            }
        };

        // Dedicated reader thread: drains stdout into a line-event channel.
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(format!("supervisor-reader-{agent_id}"))
            .spawn(move || reader_events(BufReader::new(stdout), tx, MAX_MESSAGE_BYTES))
            .map_err(|error| {
                let _ = child.kill();
                let _ = child.wait();
                SupervisorError::Spawn(std::io::Error::other(error))
            })?;

        // Send the client hello (id 0) and await the server's reply. The
        // hello declares the agent's configured whiteboard subscription
        // (ADR-60 D3): the wire field is optional (`None` back-compat), and
        // the agent consumes it in the child-side gate proxy.
        let subscriptions = self.config.whiteboard_subscriptions.get(agent_id).cloned();
        let hello = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 0,
            method: IpcMethod::Handshake,
            params: ipc::client_hello(agent_id, serde_json::json!({}), subscriptions),
        };
        let frame = ipc::serialize_frame(&serde_json::to_value(hello).map_err(|error| {
            SupervisorError::Protocol(format!("handshake serialization: {error}"))
        })?)
        .map_err(|error| SupervisorError::Protocol(error.to_string()))?;
        if let Err(error) = stdin.write_all(&frame).and_then(|_| stdin.flush()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SupervisorError::Io(error));
        }

        let waited = self.config.handshake_timeout;
        match rx.recv_timeout(waited) {
            Ok(LineEvent::Message(text)) => match interpret_handshake_reply(&text) {
                Ok(()) => {
                    let mut meta = AgentMeta::new(agent_id);
                    meta.state = AgentState::Running;
                    meta.restart_count = restart_count;
                    // The completed handshake is the spawn-time liveness
                    // proof: without this a healthy agent would be presumed
                    // stale on the loop's first tick.
                    meta.last_seen_ms = unix_ms();
                    let (reply_tx, reply_rx) = mpsc::channel();
                    self.agents.insert(
                        agent_id.to_owned(),
                        AgentProcess {
                            meta,
                            child,
                            stdin,
                            reader: ReaderHandle { _thread: thread, rx },
                            respawn: spec.clone(),
                            reply_rx,
                            reply_tx,
                            subscription_registered: false,
                        },
                    );
                    Ok(())
                }
                Err(reason) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(SupervisorError::HandshakeRejected {
                        agent_id: agent_id.to_owned(),
                        reason,
                    })
                }
            },
            Ok(LineEvent::Eof) => {
                let status = reap_soon(&mut child);
                Err(SupervisorError::ChildExited { agent_id: agent_id.to_owned(), status })
            }
            Ok(LineEvent::Error(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(SupervisorError::from(error))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(SupervisorError::HandshakeTimeout { agent_id: agent_id.to_owned(), waited })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(SupervisorError::Protocol(
                    "agent stdout reader terminated unexpectedly".to_owned(),
                ))
            }
        }
    }

    /// Stop agent `agent_id` gracefully: close its stdin (clean shutdown
    /// signal), wait out [`SupervisorConfig::kill_grace`] for a clean exit,
    /// then escalate to SIGKILL. The agent is removed from the registry either
    /// way.
    pub fn stop_agent(&mut self, agent_id: &str) -> Result<(), SupervisorError> {
        let Some(mut process) = self.agents.remove(agent_id) else {
            return Err(SupervisorError::UnknownAgent(agent_id.to_owned()));
        };
        process.meta.state = AgentState::Stopping;
        // Dropping the write end signals EOF to the child (ADR-60 D1 cleanup
        // path: stdin-close → child exits → reap).
        drop(process.stdin);

        let deadline = Instant::now() + self.config.kill_grace;
        loop {
            match process.child.try_wait() {
                Ok(Some(_status)) => {
                    process.meta.state = AgentState::Stopped;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => {
                    process.meta.state = AgentState::Failed;
                    return Err(SupervisorError::Io(error));
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Grace period exhausted: escalate.
        let _ = process.child.kill();
        match process.child.wait() {
            Ok(_status) => {
                process.meta.state = AgentState::Stopped;
                Ok(())
            }
            Err(error) => {
                process.meta.state = AgentState::Failed;
                Err(SupervisorError::Io(error))
            }
        }
    }

    /// One one_for_one restart attempt for `agent_id` (ADR-60 D1).
    ///
    /// Reaps the current child (stdin-close → grace window → SIGKILL), waits
    /// out the exponential backoff with jitter (`[restart_backoff]` +
    /// `[jitter]`, clipped to 60 s), then spawns a fresh child from the
    /// stored respawn spec.
    ///
    /// The restart budget is [`SupervisorConfig::max_restarts`]; once
    /// exhausted this returns [`SupervisorError::RestartsExhausted`] with
    /// the current child killed and reaped, and the agent's registration
    /// left in place for the caller to mark failed (its metadata survives
    /// for inspection).
    ///
    /// Blocking I/O (child reaping) happens inline — fine for the supervisor
    /// loop's single-threaded usage; revisit with `spawn_blocking` if the
    /// loop ever needs to serve many agents concurrently.
    pub async fn restart_agent(&mut self, agent_id: &str) -> Result<(), SupervisorError> {
        let restart_count = match self.agents.get(agent_id) {
            Some(process) => process.meta.restart_count,
            None => return Err(SupervisorError::UnknownAgent(agent_id.to_owned())),
        };
        if !should_restart(restart_count, self.config.max_restarts) {
            // Exhausted: tear the current child down so the refusal does not
            // leak a live agent (the EOF/stale path may still have it
            // running). The registration is left in place, its child reaped,
            // for the caller to mark failed.
            if let Some(process) = self.agents.get_mut(agent_id) {
                let _ = process.child.kill();
                let _ = process.child.wait();
            }
            return Err(SupervisorError::RestartsExhausted {
                agent_id: agent_id.to_owned(),
                restart_count,
            });
        }

        let mut process = self
            .agents
            .remove(agent_id)
            .ok_or_else(|| SupervisorError::UnknownAgent(agent_id.to_owned()))?;

        // Old child teardown: close stdin (clean-shutdown signal), give it
        // the grace window to exit on its own, then escalate to SIGKILL. The
        // old child is always reaped before respawning.
        drop(process.stdin);
        let deadline = Instant::now() + self.config.kill_grace;
        loop {
            match process.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => return Err(SupervisorError::Io(error)),
            }
            if Instant::now() >= deadline {
                let _ = process.child.kill();
                let _ = process.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let delay = jitter(
            restart_backoff(self.config.restart_backoff, restart_count, MAX_RESTART_BACKOFF),
            restart_count,
            self.config.restart_backoff_jitter,
        );
        tokio::time::sleep(delay).await;
        self.spawn_inner(&process.respawn, agent_id, restart_count + 1)
    }

    /// Liveness snapshot of every registered agent (metadata copies).
    pub fn agents(&self) -> Vec<AgentMeta> {
        self.agents.values().map(|process| process.meta.clone()).collect()
    }

    /// Steady-state operation loop (ADR-60 D1/D2): drive liveness recording
    /// and one-for-one restarts until `shutdown` fires.
    ///
    /// Every 25 ms tick the loop drains each agent's line-event channel:
    ///
    /// - heartbeat **requests** are recorded (strict per-agent sequence;
    ///   duplicates, stale sequences and clock-skewed heartbeats are
    ///   answered `accepted: false`, see [`AgentMeta::record_heartbeat`]) and
    ///   acknowledged; heartbeat **notifications** are recorded silently;
    /// - with write-path services attached
    ///   ([`Supervisor::with_services`]), `execute-tool` /
    ///   `publish-event` / `retrieve-memory` **requests** are dispatched to
    ///   the tokio task pool; the handler's reply is routed back through the
    ///   agent's reply channel and written on a later tick. Without
    ///   services they are answered `Internal` "not configured";
    /// - methods not wired are answered `MethodNotFound` with the request's
    ///   id preserved;
    /// - unparseable lines get a `ParseError` response with id 0;
    /// - a clean EOF, a dead reader channel, or a hard framing error marks
    ///   the child failed and enters the one-for-one restart policy
    ///   ([`Supervisor::restart_agent`]) — `InvalidData` framing noise is
    ///   tolerated and logged instead;
    /// - agents still [`AgentState::Running`] whose last heartbeat is older
    ///   than [`SupervisorConfig::heartbeat_timeout`] are presumed dead and
    ///   restarted the same way.
    ///
    /// When `shutdown` fires the [`RunSummary`] snapshot is captured (at the
    /// moment the loop stops driving), then every healthy agent is stopped
    /// gracefully ([`Supervisor::stop_agent`], which removes the
    /// registration); agents that failed during the run are left registered
    /// in their [`AgentState::Failed`] state for inspection.
    ///
    /// `summary.failed` lists the agents that gave up during the run (restart
    /// budget [`SupervisorConfig::max_restarts`] exhausted, or an
    /// unrecoverable spawn/io failure); `summary.agents` is the full snapshot
    /// — healthy entries carry their last loop state.
    ///
    /// Child pipe I/O and reaping are small and blocking — acceptable for the
    /// single-threaded supervisor; revisit with `spawn_blocking` if it ever
    /// needs to serve many agents concurrently.
    pub async fn run(&mut self, shutdown: CancellationToken) -> RunSummary {
        self.run_until(shutdown, |_| false).await
    }

    /// Like [`Supervisor::run`], but also returns early once `done` reports
    /// that the supervised state has reached its goal.
    ///
    /// `done` is evaluated after every tick (and after that tick's restart
    /// handling), so the loop exits at the first tick where the condition
    /// holds — this is what makes completion assertions deterministic: a
    /// caller can wait on an observed state (e.g. "both heartbeats
    /// recorded") instead of gambling on a wall-clock sleep under CPU
    /// contention. `done` must be cheap; it runs on the loop's cadence.
    /// Cancellation still ends the run regardless of `done`.
    pub async fn run_until<F>(&mut self, shutdown: CancellationToken, done: F) -> RunSummary
    where
        F: Fn(&Self) -> bool,
    {
        let hb_timeout_ms = self.config.heartbeat_timeout.as_millis() as i64;
        let services = self.services.clone();
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        let mut failed = Vec::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            let now_ms = unix_ms();
            let mut to_restart = Vec::new();
            for (agent_id, process) in self.agents.iter_mut() {
                // Failed agents are quiescent: their child is reaped, no
                // events remain meaningful, and the failure is already
                // recorded — do not re-trigger restarts for them.
                if process.meta.state == AgentState::Failed
                    || process.meta.state == AgentState::Completed
                {
                    continue;
                }
                // Async dispatch replies arrive via the reply channel; drain
                // before processing fresh events so acknowledgements stay in
                // order.
                for _ in 0..MAX_EVENTS_PER_TICK {
                    match process.reply_rx.try_recv() {
                        Ok(reply) => write_reply(&mut process.stdin, &reply),
                        Err(_) => break,
                    }
                }
                for _ in 0..MAX_EVENTS_PER_TICK {
                    match process.reader.rx.try_recv() {
                        Ok(LineEvent::Message(text)) => {
                            match dispatch_agent_line(
                                &mut process.meta,
                                &text,
                                now_ms,
                                services.as_ref(),
                            ) {
                                Dispatch::Reply(reply) => {
                                    write_reply(&mut process.stdin, &reply);
                                }
                                Dispatch::Async { id, params } => {
                                    let Some(services) = services.clone() else {
                                        // `dispatch_agent_line` only returns
                                        // `Async` with services attached, so
                                        // this branch is defensive.
                                        continue;
                                    };
                                    let tx = process.reply_tx.clone();
                                    let agent_id = agent_id.clone();
                                    let cancel = shutdown.clone();
                                    // Handlers are detached: the loop keeps
                                    // driving all agents while a write is
                                    // in flight. Replies route through the
                                    // per-agent reply channel; if the agent
                                    // is gone by then, the send just fails.
                                    tokio::spawn(async move {
                                        let response = match *params {
                                            IpcParams::ExecuteTool { request } => {
                                                handle_execute_tool(
                                                    &services, &agent_id, request, id, &cancel,
                                                )
                                                .await
                                            }
                                            IpcParams::PublishEvent { event } => {
                                                handle_publish_event(
                                                    &services, &agent_id, event, id, &cancel,
                                                )
                                                .await
                                            }
                                            IpcParams::RetrieveMemory { query, limit, .. } => {
                                                handle_retrieve_memory(
                                                    &services, &agent_id, query, limit, id, &cancel,
                                                )
                                                .await
                                            }
                                            IpcParams::ListTools { .. } => {
                                                handle_list_tools(&services, &agent_id, id).await
                                            }
                                            IpcParams::AckWhiteboard { end_gate_seq } => {
                                                handle_ack_whiteboard(
                                                    &services,
                                                    &agent_id,
                                                    end_gate_seq,
                                                    id,
                                                )
                                                .await
                                            }
                                            _ => {
                                                // Heartbeat/handshake never
                                                // reach the async path.
                                                reply_error(
                                                    id,
                                                    IpcErrorCode::InvalidRequest,
                                                    "invalid async method".to_owned(),
                                                )
                                            }
                                        };
                                        let _ = tx.send(*response);
                                    });
                                }
                                Dispatch::None => {}
                            }
                        }
                        Ok(LineEvent::Eof) => {
                            // A clean exit (code 0) is the ADR-60 S5 terminal
                            // state for a one-run-per-process agent: the task
                            // completed, so no restart. Any other exit
                            // (non-zero, or killed by signal) is a crash and
                            // goes through the one_for_one restart path.
                            if child_exited_cleanly(&mut process.child) {
                                process.meta.state = AgentState::Completed;
                            } else {
                                push_unique(&mut to_restart, agent_id);
                            }
                            break;
                        }
                        Ok(LineEvent::Error(error)) => match &error {
                            // Frame-level noise (empty line, CRLF internals):
                            // tolerate and keep draining.
                            IpcTransportError::Io(inner)
                                if inner.kind() == std::io::ErrorKind::InvalidData =>
                            {
                                tracing::debug!(%agent_id, %error, "supervisor: stray line from agent");
                            }
                            _ => push_unique(&mut to_restart, agent_id),
                        },
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            // Reader thread died without signalling EOF.
                            push_unique(&mut to_restart, agent_id);
                            break;
                        }
                    }
                }
                if process.meta.state == AgentState::Running {
                    // ADR-60 D3: (re-)register the agent's whiteboard
                    // subscription on first sighting of the current child
                    // generation (a restart inserts a fresh `AgentProcess`);
                    // the register rehydrates the persisted cursor so an
                    // agent that crashed mid-stream resumes from its last
                    // ack. Registration is guarded to agents that actually
                    // have a configured subscription: the one-time upsert
                    // must not perturb write-path timing for the plain
                    // (non-subscribed) agents. Then push one pending slice
                    // per tick.
                    if let Some(services) = &services {
                        if !process.subscription_registered
                            && self.config.whiteboard_subscriptions.contains_key(agent_id)
                        {
                            let scopes = self
                                .config
                                .whiteboard_subscriptions
                                .get(agent_id)
                                .cloned()
                                .unwrap_or_default();
                            services.subscriptions.register(agent_id.clone(), scopes).await;
                            process.subscription_registered = true;
                        }
                        supervisor_flush(services, agent_id, &mut process.stdin).await;
                    }
                    if process.meta.is_stale(now_ms, hb_timeout_ms) {
                        push_unique(&mut to_restart, agent_id);
                    }
                }
            }
            for agent_id in &to_restart {
                match self.restart_agent(agent_id).await {
                    Ok(()) => {}
                    Err(SupervisorError::RestartsExhausted { .. }) => {
                        tracing::warn!(%agent_id, "supervisor: restart budget exhausted; agent failed");
                        self.mark_failed(agent_id);
                        failed.push(agent_id.clone());
                    }
                    Err(SupervisorError::UnknownAgent(_)) => {}
                    Err(error) => {
                        tracing::warn!(%agent_id, %error, "supervisor: restart attempt failed");
                        self.mark_failed(agent_id);
                        failed.push(agent_id.clone());
                    }
                }
            }
            // Completion predicate (see `run_until`): checked after the
            // tick's events and restarts are fully applied.
            if done(self) {
                break;
            }
        }
        // Shutdown drain: each agent's reader thread buffers the child's
        // output independently of this loop, so a load-starved loop must
        // still account for everything already received — otherwise
        // heartbeats and a terminal EOF would silently vanish from the
        // summary (flaky under heavy parallel test load).
        // Teardown semantics: no restarts, no new async work — in-flight
        // work already detached keeps running and its replies are drained.
        let now_ms = unix_ms();
        let mut shutdown_failed = Vec::new();
        for process in self.agents.values_mut() {
            // Terminal agents stay as they are: any lines still queued belong
            // to replaced child generations (an EOF from a child that was
            // already restarted away must not re-classify the agent).
            if matches!(process.meta.state, AgentState::Failed | AgentState::Completed) {
                continue;
            }
            // Replies from detached in-flight handlers are moot at teardown.
            while process.reply_rx.try_recv().is_ok() {}
            // Grace drain: block briefly on the reader channel so a line the
            // child already wrote but the reader had not yet delivered (e.g.
            // the writer process was briefly starved of CPU) is captured in
            // the summary instead of silently dropped. Bounded: a quiet
            // channel ends the drain immediately.
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match process.reader.rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(LineEvent::Message(text)) => {
                        match dispatch_agent_line(
                            &mut process.meta,
                            &text,
                            now_ms,
                            services.as_ref(),
                        ) {
                            Dispatch::Reply(reply) => write_reply(&mut process.stdin, &reply),
                            // Do not start new gated work during teardown.
                            Dispatch::Async { id, .. } => write_reply(
                                &mut process.stdin,
                                &reply_error(
                                    id,
                                    IpcErrorCode::Internal,
                                    "supervisor shutting down".to_owned(),
                                ),
                            ),
                            Dispatch::None => {}
                        }
                    }
                    Ok(LineEvent::Eof) => {
                        // Mirror the tick's terminal classification: a clean
                        // exit is the completed task; anything else is a
                        // crash, recorded as failed (no restarts at teardown).
                        if child_exited_cleanly(&mut process.child) {
                            process.meta.state = AgentState::Completed;
                        } else {
                            process.meta.state = AgentState::Failed;
                            process.meta.failed_at_ms = Some(now_ms);
                            shutdown_failed.push(process.meta.agent_id.clone());
                        }
                        break;
                    }
                    Ok(LineEvent::Error(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if Instant::now() >= deadline {
                            break;
                        }
                    }
                }
            }
        }
        failed.extend(shutdown_failed);
        let summary = RunSummary { failed: failed.clone(), agents: self.agents() };
        for agent_id in self.agents.keys().map(Clone::clone).collect::<Vec<_>>() {
            if failed.contains(&agent_id) {
                continue;
            }
            // Completed agents are already reaped and terminal; stopping them
            // (stdin-close → grace → kill) would only overwrite the state.
            if self.agents.get(&agent_id).is_some_and(|p| p.meta.state == AgentState::Completed) {
                continue;
            }
            let _ = self.stop_agent(&agent_id);
        }
        summary
    }

    /// Mark `agent_id` failed and record when. The (dead or dying)
    /// registration is left in place for inspection.
    pub fn mark_failed(&mut self, agent_id: &str) {
        if let Some(process) = self.agents.get_mut(agent_id) {
            process.meta.state = AgentState::Failed;
            process.meta.failed_at_ms = Some(unix_ms());
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Best-effort: nobody should outlive their supervisor. The reader
        // thread exits on its own once the child's stdout closes.
        for process in self.agents.values_mut() {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }
}

/// Parse one handshake reply line into an outcome.
///
/// `Ok(())` means the peer announced our [`PROTOCOL_VERSION`](crate::ipc::PROTOCOL_VERSION)
/// and accepted the connection; anything else is a `Err(reason)` for
/// [`SupervisorError::HandshakeRejected`].
/// Poll the child's exit status with a short deadline: `true` iff it has
/// exited with code 0. Used to classify a stdout EOF as the ADR-60 S5
/// terminal condition (clean exit → `Completed`) versus a crash (restart).
fn child_exited_cleanly(child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code() == Some(0),
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn interpret_handshake_reply(text: &str) -> Result<(), String> {
    let reply: crate::ipc::IpcResponse = serde_json::from_str(text)
        .map_err(|error| format!("unparseable handshake reply: {error}"))?;
    if reply.id != 0 {
        return Err(format!("handshake reply carried unexpected id {}", reply.id));
    }
    if let Some(error) = reply.error {
        return Err(format!("handshake error {}: {}", i32::from(error.code), error.message));
    }
    match reply.result {
        Some(IpcResult::Handshake { protocol_version: peer, accepted, .. }) => {
            if !accepted {
                return Err(format!("peer refused handshake (speaks {peer})"));
            }
            crate::ipc::validate_version(&peer).map_err(|error| error.message)
        }
        other => Err(format!("handshake reply carried unexpected result: {other:?}")),
    }
}

/// Reap a child that already signalled EOF on stdout without hanging if it
/// merely closed the pipe while staying alive: poll briefly, then kill.
fn reap_soon(child: &mut Child) -> String {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.to_string(),
            Ok(None) => {}
            Err(error) => return format!("reap error: {error}"),
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    child
        .wait()
        .map(|status| status.to_string())
        .unwrap_or_else(|error| format!("reap error: {error}"))
}

/// Max line events drained per agent per tick: a per-agent backpressure
/// bound so one chatty agent cannot starve the others.
const MAX_EVENTS_PER_TICK: usize = 64;

/// Push `value` only if not already present (one agent can be flagged by
/// several signals in the same tick).
fn push_unique(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|existing| existing == value) {
        list.push(value.to_owned());
    }
}

/// Unix epoch milliseconds (UTC), `0` if the clock is unavailable.
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// What the loop should do with one decoded agent line.
#[derive(Debug)]
enum Dispatch {
    /// Write this reply synchronously (heartbeat acks, parse errors, and the
    /// no-services case).
    Reply(Box<IpcResponse>),
    /// Handle on the async task pool (gate/whiteboard/memory); the reply
    /// arrives via the agent's reply channel once the handler completes.
    Async { id: u64, params: Box<IpcParams> },
    /// Nothing to do (notifications, ignored handshakes).
    None,
}

/// Steady-state policy for one decoded line from an agent (ADR-60 D2):
/// what the loop should do with it.
///
/// Requests are tried first (a request also decodes as a notification).
/// Heartbeat requests are recorded and acknowledged (`accepted` reflects
/// [`AgentMeta::record_heartbeat`]); a heartbeat with malformed params is
/// answered `InvalidParams`. The write-path methods (`ExecuteTool` /
/// `PublishEvent` / `RetrieveMemory`) are dispatched to the async task pool
/// when `services` are attached and answered `Internal` "not configured"
/// otherwise — the fail-closed posture until the supervisor is wired. A
/// second handshake is ignored. Heartbeat notifications are recorded
/// silently; other notifications are ignored. Content that is neither is
/// answered `ParseError` with id 0.
fn dispatch_agent_line(
    meta: &mut AgentMeta,
    text: &str,
    now_ms: i64,
    services: Option<&SupervisorServices>,
) -> Dispatch {
    if let Ok(request) = serde_json::from_str::<IpcRequest>(text) {
        return match request.method {
            IpcMethod::Handshake => Dispatch::None,
            IpcMethod::Heartbeat => match &request.params {
                IpcParams::Heartbeat { seq, .. } => {
                    // Liveness anchors on THIS process's clock (`now_ms`),
                    // matching the handshake's `last_seen_ms` stamp — never
                    // on the agent's self-reported wire `timestamp_ms`,
                    // which comes from another process's clock and can sit
                    // milliseconds behind the supervisor's sample of its
                    // own, spuriously tripping ClockWentBackwards and
                    // silently dropping healthy heartbeats.
                    let accepted = meta.record_heartbeat(*seq, now_ms, 0).is_ok();
                    Dispatch::Reply(reply_ok(request.id, IpcResult::Heartbeat { accepted }))
                }
                _ => Dispatch::Reply(reply_error(
                    request.id,
                    IpcErrorCode::InvalidParams,
                    "heartbeat params required".to_owned(),
                )),
            },
            IpcMethod::ExecuteTool
            | IpcMethod::PublishEvent
            | IpcMethod::RetrieveMemory
            | IpcMethod::ListTools
            | IpcMethod::AckWhiteboard => match services {
                Some(_) => Dispatch::Async { id: request.id, params: Box::new(request.params) },
                None => Dispatch::Reply(reply_error(
                    request.id,
                    IpcErrorCode::Internal,
                    "supervisor write services are not configured".to_owned(),
                )),
            },
            // `whiteboard-slice` is a supervisor→agent notification only; a
            // request-shaped line with that method is protocol noise.
            IpcMethod::WhiteboardSlice => Dispatch::Reply(reply_error(
                request.id,
                IpcErrorCode::InvalidRequest,
                "whiteboard-slice is a supervisor-to-agent notification".to_owned(),
            )),
        };
    }
    if let Ok(notification) = serde_json::from_str::<IpcNotification>(text) {
        if notification.method == IpcMethod::Heartbeat {
            if let IpcParams::Heartbeat { seq, .. } = notification.params {
                // Same clock rule as the request path: record against this
                // process's `now_ms`, not the agent's wire timestamp (see
                // the comment there).
                let _ = meta.record_heartbeat(seq, now_ms, 0);
            }
        }
        return Dispatch::None;
    }
    Dispatch::Reply(reply_error(0, IpcErrorCode::ParseError, "unparseable line".to_owned()))
}

/// A success response with `result` and no error.
fn reply_ok(id: u64, result: IpcResult) -> Box<IpcResponse> {
    Box::new(IpcResponse { jsonrpc: "2.0".to_owned(), id, result: Some(result), error: None })
}

/// An error response with no result.
fn reply_error(id: u64, code: IpcErrorCode, message: String) -> Box<IpcResponse> {
    Box::new(IpcResponse {
        jsonrpc: "2.0".to_owned(),
        id,
        result: None,
        error: Some(IpcError::new(code, message)),
    })
}

/// An error response carrying a prebuilt error (e.g. a gate-mapped one with
/// its own message).
fn reply_error_full(id: u64, error: IpcError) -> Box<IpcResponse> {
    Box::new(IpcResponse { jsonrpc: "2.0".to_owned(), id, result: None, error: Some(error) })
}

/// Execute one gated write off the loop (ADR-60 D4).
///
/// Attribution: `request.agent_id` is bound to the registered agent id at the
/// process boundary — the wire value is never trusted. The gate applies
/// policy, assigns `gate_seq` in the whiteboard log, executes the tool, and
/// persists the applied event before the agent is acknowledged.
async fn handle_execute_tool(
    services: &SupervisorServices,
    agent_id: &str,
    mut request: GateRequest,
    id: u64,
    cancel: &CancellationToken,
) -> Box<IpcResponse> {
    request.agent_id = agent_id.to_owned();
    // ADR-60 D5 always-on: the supervisor attests each mutated target's
    // current state at request arrival so every versioned write carries
    // per-target `base_versions` claims (see [`crate::gate::stamp_base_versions`]);
    // a caller-declared claim is never clobbered, per target.
    stamp_base_versions(&services.gate, &mut request).await;
    match services.gate.submit(request, cancel.clone()).await {
        Ok(outcome) => {
            // Wake subscribers: a gated write appended a `write-applied`
            // event at `outcome.gate_seq` (the gate commits the WAL row
            // itself); the publisher needs no re-read — `gate_seq` is the
            // wake coordinate.
            services.subscriptions.mark_append(outcome.gate_seq).await;
            // ADR-60 D6: feed the consolidation trigger. The pass detaches
            // onto the runtime — this reply path never awaits indexing.
            if let Some(consolidator) = &services.consolidation {
                consolidator.note_append(cancel.clone());
            }
            reply_ok(id, IpcResult::ExecuteTool { outcome })
        }
        Err(error) => {
            // ADR-60 D5: a base_version collision is the loud signal that
            // sibling agents raced on shared files — record it at warn so
            // the operator/supervisor has a manual-resolution trail even
            // though no whiteboard row was written.
            if let GateError::Conflict { event_id, reason } = &error {
                tracing::warn!(%agent_id, %event_id, %reason, "supervisor: optimistic write conflict");
            }
            reply_error_full(id, IpcError::from_gate(&error))
        }
    }
}

/// Append one agent-attested event to the whiteboard log (ADR-60 D3); the
/// log assigns `gate_seq` (global) and `agent_seq` (per agent). The agent id
/// is bound to the registered process, never trusted from the wire. A
/// committed append wakes subscribed peers so their slices flow on a later
/// tick, and counts toward the D6 consolidation trigger.
async fn handle_publish_event(
    services: &SupervisorServices,
    agent_id: &str,
    mut event: NewWhiteboardEvent,
    id: u64,
    cancel: &CancellationToken,
) -> Box<IpcResponse> {
    event.agent_id = agent_id.to_owned();
    match append_whiteboard_event(&services.whiteboard_pool, &event).await {
        Ok(stored) => {
            services.subscriptions.mark_append(stored.gate_seq).await;
            // ADR-60 D6: same out-of-band trigger as the gated-write path.
            if let Some(consolidator) = &services.consolidation {
                consolidator.note_append(cancel.clone());
            }
            reply_ok(id, IpcResult::PublishEvent { stored })
        }
        Err(error) => reply_error_full(
            id,
            IpcError::new(IpcErrorCode::Internal, format!("whiteboard append failed: {error}")),
        ),
    }
}

/// Persist an acknowledged consistent-cut coordinate (ADR-60 D3): the
/// per-subscriber cursor advances only on `ack-whiteboard`, never at
/// enqueue/flush, so an overflow or crash merely stalls delivery and the
/// agent's retry loop resumes from the last acked cut. The reply echoes the
/// coordinate so the agent can correlate retries.
async fn handle_ack_whiteboard(
    services: &SupervisorServices,
    agent_id: &str,
    end_gate_seq: u64,
    id: u64,
) -> Box<IpcResponse> {
    services.subscriptions.ack(agent_id, end_gate_seq).await;
    reply_ok(id, IpcResult::AckWhiteboard { end_gate_seq })
}

/// Publish the supervisor's tool registry to an agent (ADR-60 S5): the gate
/// owns the registry the agents execute under, so it is the single source of
/// truth for what an agent may present to the model. The wire agent id is
/// ignored; the registered process is the authority.
async fn handle_list_tools(
    services: &SupervisorServices,
    _agent_id: &str,
    id: u64,
) -> Box<IpcResponse> {
    reply_ok(id, IpcResult::ListTools { tools: services.gate.tool_definitions() })
}

/// Query the memory spine off the loop (ADR-60 D6). The query is scoped to
/// the supervisor's project namespace; the wire agent id is ignored. The
/// shortlist is clamped to [`crate::consolidation::DISCLOSURE_MAX_CHUNKS`] —
/// one disclosure level with a bounded 5–10-chunk window, never unbounded
/// retrieval.
async fn handle_retrieve_memory(
    services: &SupervisorServices,
    _agent_id: &str,
    query_text: String,
    limit: u32,
    id: u64,
    cancel: &CancellationToken,
) -> Box<IpcResponse> {
    let top_k = (limit.max(1) as usize).min(crate::consolidation::DISCLOSURE_MAX_CHUNKS);
    let query = MemoryQuery {
        text: query_text,
        project_id: services.project_id.clone(),
        namespace: MemoryNamespace::Project(services.project_id.clone()),
        top_k,
        filters: Vec::new(),
    };
    match services.memory.retrieve(&query, cancel.clone()).await {
        Ok(chunks) => {
            // Wire chunks carry the retrieval score and a kind label; the
            // whiteboard linkage is resolved by the memory layer in a later
            // chunk (ADR-60 D6), so `source_event_id` is unknown here.
            let chunks = chunks
                .into_iter()
                .map(|chunk| crate::ipc::MemoryChunk {
                    text: chunk.content,
                    kind: format!("{:?}", chunk.chunk_type).to_lowercase(),
                    score: chunk.score,
                    source_event_id: None,
                })
                .collect();
            reply_ok(id, IpcResult::RetrieveMemory { chunks })
        }
        Err(error) => reply_error_full(
            id,
            IpcError::new(IpcErrorCode::Internal, format!("memory retrieval failed: {error}")),
        ),
    }
}

/// Write one reply line to an agent's stdin; failures are logged — the
/// draining side owns the failure policy.
fn write_reply(stdin: &mut std::process::ChildStdin, reply: &IpcResponse) {
    let value = match serde_json::to_value(reply) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "supervisor: reply serialization failed");
            return;
        }
    };
    let frame = match ipc::serialize_frame(&value) {
        Ok(frame) => frame,
        Err(error) => {
            tracing::warn!(%error, "supervisor: reply framing failed");
            return;
        }
    };
    if let Err(error) = IoWrite::write_all(stdin, &frame).and_then(|()| IoWrite::flush(stdin)) {
        tracing::warn!(%error, "supervisor: reply write failed (agent gone?)");
    }
}

/// Write one notification line (e.g. `whiteboard-slice`) to an agent's
/// stdin; failures are logged — the draining side owns the failure policy.
/// Notification writes race nothing: the loop is the agent's single writer.
fn write_notification(stdin: &mut std::process::ChildStdin, notification: &IpcNotification) {
    let value = match serde_json::to_value(notification) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "supervisor: notification serialization failed");
            return;
        }
    };
    let frame = match ipc::serialize_frame(&value) {
        Ok(frame) => frame,
        Err(error) => {
            tracing::warn!(%error, "supervisor: notification framing failed");
            return;
        }
    };
    if let Err(error) = IoWrite::write_all(stdin, &frame).and_then(|()| IoWrite::flush(stdin)) {
        tracing::warn!(%error, "supervisor: notification write failed (agent gone?)");
    }
}

/// Push one pending whiteboard slice per dirty subscription of `agent_id`
/// (ADR-60 D3, supervisor side). Bounded per tick: a slice that reports
/// `more` (window full / byte-capped) stays dirty and is re-pushed on the
/// next tick, so a large backlog drains across ticks instead of
/// monopolizing one. Runs only for `Running` agents (checked by the loop).
async fn supervisor_flush(
    services: &SupervisorServices,
    agent_id: &str,
    stdin: &mut std::process::ChildStdin,
) {
    for candidate in services.subscriptions.flush_candidates().await {
        if candidate != agent_id {
            continue;
        }
        let Some(batch) = services.subscriptions.pending_slice(agent_id).await else {
            continue;
        };
        write_notification(
            stdin,
            &IpcNotification {
                jsonrpc: "2.0".to_owned(),
                method: IpcMethod::WhiteboardSlice,
                params: IpcParams::WhiteboardSlice {
                    subscription_id: agent_id.to_owned(),
                    events: batch.events,
                    end_gate_seq: batch.end_gate_seq,
                },
            },
        );
        services.subscriptions.mark_flushed(agent_id, batch.end_gate_seq, batch.more).await;
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    /// Assert the event list is exactly `[Message(a), ..., Eof]`, matching
    /// events structurally (no `PartialEq` on event types).
    fn assert_messages_then_eof(events: Vec<LineEvent>, expected: &[&str]) {
        assert_eq!(events.len(), expected.len() + 1, "messages + trailing Eof");
        for (event, text) in events.iter().take(expected.len()).zip(expected) {
            assert!(
                matches!(event, LineEvent::Message(got) if got == text),
                "expected {text:?}, got {event:?}"
            );
        }
        assert!(
            matches!(events.last(), Some(LineEvent::Eof)),
            "expected trailing Eof, got {events:?}"
        );
    }

    #[test]
    fn reader_two_messages_then_eof() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = b"{\"a\":1}\n{\"b\":2}\n";
        reader_events(bytes, tx, MAX_MESSAGE_BYTES);
        assert_messages_then_eof(rx.try_iter().collect(), &["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn reader_final_unterminated_line_is_delivered() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = b"{\"a\":1}";
        reader_events(bytes, tx, MAX_MESSAGE_BYTES);
        assert_messages_then_eof(rx.try_iter().collect(), &["{\"a\":1}"]);
    }

    #[test]
    fn reader_strips_crlf() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = b"{\"a\":1}\r\n";
        reader_events(bytes, tx, MAX_MESSAGE_BYTES);
        assert_messages_then_eof(rx.try_iter().collect(), &["{\"a\":1}"]);
    }

    #[test]
    fn reader_empty_line_is_invalid_data() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = b"\n";
        reader_events(bytes, tx, MAX_MESSAGE_BYTES);
        let events = rx.try_iter().collect::<Vec<_>>();
        assert!(
            matches!(events[0], LineEvent::Error(IpcTransportError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidData),
            "expected InvalidData error, got {:?}",
            events[0]
        );
        assert!(matches!(events[1], LineEvent::Eof), "expected trailing Eof, got {:?}", events[1]);
    }

    #[test]
    fn reader_oversized_line_is_reported() {
        let (tx, rx) = mpsc::channel();
        let bytes = vec![b'a'; 65];
        reader_events(bytes.as_slice(), tx, 32);
        let events = rx.try_iter().collect::<Vec<_>>();
        assert!(
            matches!(
                events[0],
                LineEvent::Error(IpcTransportError::Oversized { len: 65, max_len: 32 })
            ),
            "expected Oversized, got {:?}",
            events[0]
        );
        assert!(matches!(events[1], LineEvent::Eof), "expected trailing Eof, got {:?}", events[1]);
    }

    #[test]
    fn reader_invalid_utf8_is_reported() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = &[0xff, 0xfe, b'\n'];
        reader_events(bytes, tx, MAX_MESSAGE_BYTES);
        let events = rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(events[0], LineEvent::Error(IpcTransportError::InvalidUtf8)));
        assert!(matches!(events[1], LineEvent::Eof), "expected trailing Eof, got {:?}", events[1]);
    }
}

#[cfg(test)]
mod run_loop_tests {
    use super::*;

    /// Unwrap a sync reply; panics if the dispatch was async or silent.
    fn reply_of(dispatch: Dispatch) -> IpcResponse {
        match dispatch {
            Dispatch::Reply(reply) => *reply,
            other => panic!("expected a sync reply, got {other:?}"),
        }
    }

    fn heartbeat_request(id: u64, seq: u64, timestamp_ms: i64) -> String {
        serde_json::to_string(&IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id,
            method: IpcMethod::Heartbeat,
            params: IpcParams::Heartbeat {
                agent_id: "agent-a".to_owned(),
                seq,
                timestamp_ms,
                status: "ready".to_owned(),
            },
        })
        .expect("serialize")
    }

    fn heartbeat_notification(seq: u64, timestamp_ms: i64) -> String {
        serde_json::to_string(&IpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: IpcMethod::Heartbeat,
            params: IpcParams::Heartbeat {
                agent_id: "agent-a".to_owned(),
                seq,
                timestamp_ms,
                status: "ready".to_owned(),
            },
        })
        .expect("serialize")
    }

    #[test]
    fn heartbeat_request_is_recorded_and_acknowledged() {
        let mut meta = AgentMeta::new("agent-a");
        let reply =
            reply_of(dispatch_agent_line(&mut meta, &heartbeat_request(7, 1, 100), 1_000, None));
        assert_eq!(reply.id, 7);
        assert!(matches!(reply.result.as_ref(), Some(IpcResult::Heartbeat { accepted: true })));
        assert_eq!(meta.seq, 1);
    }

    #[test]
    fn stale_heartbeat_request_is_answered_unaccepted() {
        let mut meta = AgentMeta::new("agent-a");
        meta.record_heartbeat(5, 500, 1_000).expect("record");
        let reply =
            reply_of(dispatch_agent_line(&mut meta, &heartbeat_request(7, 3, 300), 1_000, None));
        assert!(matches!(reply.result.as_ref(), Some(IpcResult::Heartbeat { accepted: false })));
    }

    #[test]
    fn heartbeat_notification_is_recorded_silently() {
        let mut meta = AgentMeta::new("agent-a");
        let dispatch = dispatch_agent_line(&mut meta, &heartbeat_notification(1, 100), 1_000, None);
        assert!(matches!(dispatch, Dispatch::None), "notifications get no response");
        assert_eq!(meta.seq, 1);
    }

    #[test]
    fn write_method_without_services_is_answered_internal_not_async() {
        // No services attached (the default `Supervisor::new`): the wired
        // write-path methods fail closed with an Internal error instead of
        // being silently dropped or dispatched.
        let mut meta = AgentMeta::new("agent-a");
        let text = serde_json::to_string(&IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 42,
            method: IpcMethod::RetrieveMemory,
            params: IpcParams::RetrieveMemory {
                query: "q".to_owned(),
                agent_id: "agent-a".to_owned(),
                limit: 3,
            },
        })
        .expect("serialize");
        let reply = reply_of(dispatch_agent_line(&mut meta, &text, 1_000, None));
        assert_eq!(reply.id, 42);
        let error = reply.error.as_ref().expect("an error");
        assert_eq!(error.code, IpcErrorCode::Internal);
        assert!(error.message.contains("not configured"), "message {:?}", error.message);
    }

    #[test]
    fn unparseable_line_gets_parse_error_with_id_zero() {
        let mut meta = AgentMeta::new("agent-a");
        let reply = reply_of(dispatch_agent_line(&mut meta, "not json at all", 1_000, None));
        assert_eq!(reply.id, 0);
        assert_eq!(reply.error.as_ref().expect("an error").code, IpcErrorCode::ParseError);
    }

    #[test]
    fn second_handshake_is_ignored() {
        let mut meta = AgentMeta::new("agent-a");
        let text = serde_json::to_string(&IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 0,
            method: IpcMethod::Handshake,
            params: ipc::client_hello("agent-a", serde_json::json!({}), None),
        })
        .expect("serialize");
        assert!(matches!(dispatch_agent_line(&mut meta, &text, 1_000, None), Dispatch::None));
    }
}

/// Write-path handler tests: ADR-60 D5 always-on `base_versions` injection in
/// the supervised write path (see [`crate::gate::stamp_base_versions`]).
///
/// These are deterministic where the child-process e2e cannot be: the
/// interleaving that turns an injected claim into a conflict — a sibling write
/// landing between the supervisor's stamp and the gate's own pre-image capture
/// — is driven directly instead of raced on wall-clock.
#[cfg(test)]
mod write_path_tests {
    use super::*;
    use crate::gate::{stamp_base_versions, FilePreImageReader};
    use async_trait::async_trait;
    use concerto_core::error::{MemoryError, PolicyError};
    use concerto_core::executor::ToolExecutor;
    use concerto_core::memory::{MemoryChunk, MemoryEntry, MemoryId, MemoryQuery, ProjectId};
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::memory::MemoryStore;
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
    use concerto_sessions::whiteboard::{
        load_whiteboard_events, load_whiteboard_subscription, WhiteboardEvent, WhiteboardKind,
        WhiteboardLoadOpts, WhiteboardScope,
    };
    use concerto_tools::filesystem::FilesystemTool;
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use ulid::Ulid;

    /// The `Decision` topic scope used by subscription tests.
    fn decision_scope() -> Vec<WhiteboardScope> {
        vec![WhiteboardScope { topics: vec![WhiteboardKind::Decision] }]
    }

    /// No-op audit log (mirrors the gate's own test stub).
    struct TestAudit;

    #[async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    fn allow_engine() -> Arc<SimplePolicyEngine> {
        Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(TestAudit),
        ))
    }

    /// Memory spine stub — `handle_execute_tool` never touches it, but
    /// `SupervisorServices` requires one.
    struct CountingMemoryStore;

    #[async_trait]
    impl MemoryStore for CountingMemoryStore {
        async fn retrieve(
            &self,
            _query: &MemoryQuery,
            _cancel: CancellationToken,
        ) -> Result<Vec<MemoryChunk>, MemoryError> {
            Ok(Vec::new())
        }
        async fn store(
            &self,
            _entry: MemoryEntry,
            _cancel: CancellationToken,
        ) -> Result<MemoryId, MemoryError> {
            Ok(MemoryId(concerto_core::ids::Ulid::new()))
        }
        async fn invalidate(
            &self,
            _id: MemoryId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    /// A gate whose executor is the REAL `FilesystemTool` rooted at `root` —
    /// the shape the supervisor builds in production.
    fn fs_gate(pool: sqlx::SqlitePool, root: PathBuf) -> Arc<WriteGate> {
        let mut registry = ToolRegistry::default();
        let utf8_root =
            camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir root is utf-8");
        registry.register(Box::new(FilesystemTool::new(utf8_root)));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), allow_engine()));
        Arc::new(WriteGate::new(
            allow_engine(),
            executor,
            pool,
            Arc::new(FilePreImageReader::new(root.clone())),
            root,
            1,
        ))
    }

    /// File-backed pool with the same PRAGMAs as production (WAL,
    /// busy_timeout, synchronous=NORMAL) and all sessions migrations applied.
    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("supervisor_write_path.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(6)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn services(pool: sqlx::SqlitePool, root: PathBuf) -> SupervisorServices {
        SupervisorServices {
            gate: fs_gate(pool.clone(), root),
            whiteboard_pool: pool.clone(),
            memory: Arc::new(CountingMemoryStore),
            project_id: ProjectId("proj-d5-inject".to_owned()),
            subscriptions: SubscriptionManager::new(pool),
            consolidation: None,
        }
    }

    fn fs_write(call_id: &str, path: &str, content: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: "wire-value-ignored".to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({ "operation": "write", "path": path, "content": content }),
            session_id: None,
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
        }
    }

    /// All whiteboard rows in `gate_seq` order.
    async fn all_events(pool: &sqlx::SqlitePool) -> Vec<WhiteboardEvent> {
        load_whiteboard_events(pool, &WhiteboardLoadOpts::default()).await.expect("load events")
    }

    #[tokio::test]
    async fn stamp_base_versions_injects_fresh_hashes_and_never_clobbers_declared_claims() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool, root.clone());

        // A versioned write with no claim gets the arrival-time pre-image
        // hash stamped in (the always-on injection).
        let mut undecided = fs_write("stamp-a", "f.txt", "new");
        stamp_base_versions(&services.gate, &mut undecided).await;
        assert_eq!(
            undecided.base_versions.get("f.txt").map(String::as_str),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "the supervisor stamps the target's current pre-image hash"
        );

        // A caller-declared claim always wins — never clobbered, even when
        // it is stale (the gate will surface the conflict, not the stamp).
        let mut declared = fs_write("stamp-b", "f.txt", "new");
        declared.base_versions.insert("f.txt".to_owned(), "declared-stale".to_owned());
        stamp_base_versions(&services.gate, &mut declared).await;
        assert_eq!(
            declared.base_versions.get("f.txt").map(String::as_str),
            Some("declared-stale"),
            "a declared base_version is never overwritten by the injection"
        );

        // A move is stamped on BOTH mutated targets (source + destination):
        // each existing target gets its current pre-image claim.
        std::fs::write(root.join("src.txt"), "move me").expect("seed move source");
        let mut move_req = fs_write("stamp-e1", "f.txt", "x");
        move_req.input = json!({
            "operation": "move", "path": "src.txt", "destination": "dest.txt"
        });
        stamp_base_versions(&services.gate, &mut move_req).await;
        assert_eq!(
            move_req.base_versions.get("src.txt").map(String::as_str),
            Some(blake3::hash(b"move me").to_hex().to_string().as_str()),
            "a move's (mutated) source is stamped"
        );
        assert!(
            !move_req.base_versions.contains_key("dest.txt"),
            "a fresh move destination has no prior version to claim"
        );

        // Declare-wins is PER TARGET: a declared claim on the source is kept
        // while the undeclared destination is still stamped.
        let mut partial = fs_write("stamp-e2", "f.txt", "x");
        partial.input =
            json!({ "operation": "move", "path": "src.txt", "destination": "dest.txt" });
        partial.base_versions.insert("src.txt".to_owned(), "declared-stale".to_owned());
        stamp_base_versions(&services.gate, &mut partial).await;
        assert_eq!(
            partial.base_versions.get("src.txt").map(String::as_str),
            Some("declared-stale"),
            "a declared claim on one target is never clobbered"
        );
        assert!(
            !partial.base_versions.contains_key("dest.txt"),
            "the other (fresh) target still ends up claim-free"
        );
    }

    #[tokio::test]
    async fn stamp_base_versions_leaves_non_versioned_and_read_only_targets_untouched() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool, root.clone());

        // A non-versioned operation (`list`) and a non-filesystem tool get no
        // claim: stamping must not change the request at all.
        let mut listed = fs_write("stamp-c", "f.txt", "x");
        listed.input = json!({ "operation": "list", "path": "." });
        stamp_base_versions(&services.gate, &mut listed).await;
        assert!(listed.base_versions.is_empty(), "non-versioned operation carries no claim");

        let mut shell = fs_write("stamp-d", "f.txt", "x");
        shell.tool = "shell".to_owned();
        stamp_base_versions(&services.gate, &mut shell).await;
        assert!(shell.base_versions.is_empty(), "non-filesystem tool carries no claim");

        // A copy stamps only its DESTINATION: the source is read-only by
        // design (see `versioned_targets`), so it is never claim-stamped.
        std::fs::write(root.join("orig.txt"), "copy me").expect("seed copy source");
        let mut copy_req = fs_write("stamp-f", "f.txt", "x");
        copy_req.input =
            json!({ "operation": "copy", "path": "orig.txt", "destination": "out.txt" });
        stamp_base_versions(&services.gate, &mut copy_req).await;
        assert!(
            !copy_req.base_versions.contains_key("orig.txt"),
            "the read-only copy source is not stamped as a claim"
        );
        assert!(
            !copy_req.base_versions.contains_key("out.txt"),
            "a fresh copy destination has no prior version to claim"
        );
    }

    #[tokio::test]
    async fn handle_execute_tool_stamps_and_applies_a_stable_write() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool.clone(), root.clone());
        let cancel = CancellationToken::new();

        let response =
            handle_execute_tool(&services, "agent-a", fs_write("h-1", "f.txt", "new"), 1, &cancel)
                .await;
        assert!(response.error.is_none(), "applied, got {:?}", response.error);

        // The injected claim matched the gate's own re-read: the write applied
        // and materialized, and the row records the stamped pre-write hash.
        let events = all_events(&pool).await;
        let applied: Vec<_> =
            events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
        assert_eq!(applied.len(), 1, "exactly one applied write");
        assert_eq!(applied[0].event_id, "h-1");
        assert_eq!(
            applied[0].pre_image_hash.as_deref(),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "the row records the injected pre-write hash"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "new",
            "the injected-base write materializes"
        );
    }

    #[tokio::test]
    async fn handle_execute_tool_surfaces_conflict_when_sibling_write_lands_between_stamp_and_gate()
    {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool.clone(), root.clone());
        let cancel = CancellationToken::new();

        // The supervisor stamps the arrival-time claim...
        let mut request = fs_write("h-2", "f.txt", "mine");
        stamp_base_versions(&services.gate, &mut request).await;
        assert_eq!(
            request.base_versions.get("f.txt").map(String::as_str),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "claim stamped at request arrival"
        );

        // ...then a sibling write lands on the same target before the gate
        // processes the request. The injected claim is now stale, so the write
        // must surface as a loud `Conflict` (IpcErrorCode::Conflict), not
        // silently overwrite the sibling.
        std::fs::write(root.join("f.txt"), "sibling-interloper").expect("sibling write");
        let response = handle_execute_tool(&services, "agent-b", request, 7, &cancel).await;
        let error = response.error.as_ref().expect("conflict surfaced to the agent");
        assert_eq!(
            error.code,
            IpcErrorCode::Conflict,
            "a stale injected claim must surface as Conflict, got {error:?}"
        );
        assert!(
            error.message.contains("base_version mismatch"),
            "the message explains the race: {:?}",
            error.message
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "sibling-interloper",
            "the sibling's write is untouched"
        );
        let events = all_events(&pool).await;
        assert!(
            !events.iter().any(|e| e.event_id == "h-2"),
            "a conflicted write appends nothing to the whiteboard"
        );
    }

    #[tokio::test]
    async fn handle_execute_tool_declared_stale_claim_is_not_clobbered() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool.clone(), root.clone());
        let cancel = CancellationToken::new();

        // The agent declares a stale claim: the injection must NOT overwrite it
        // with the current hash — the declaration wins, and the gate refuses.
        let mut request = fs_write("h-3", "f.txt", "hijack");
        request
            .base_versions
            .insert("f.txt".to_owned(), blake3::hash(b"an-outdated-view").to_hex().to_string());
        let response = handle_execute_tool(&services, "agent-a", request, 8, &cancel).await;
        let error = response.error.as_ref().expect("declared stale claim refused");
        assert_eq!(error.code, IpcErrorCode::Conflict, "declared claim honored, got {error:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "base",
            "the refused write never reached disk"
        );
    }

    // ---------------------------------------------------------------------
    // ADR-60 D3 whiteboard subscription push (supervisor-side handlers).
    // ---------------------------------------------------------------------

    fn new_event(agent: &str, kind: WhiteboardKind, note: &str) -> NewWhiteboardEvent {
        NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: agent.to_owned(),
            kind,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: serde_json::json!({ "note": note }),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn handle_ack_whiteboard_persists_the_cut_and_echoes_the_coordinate() {
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool.clone(), std::env::temp_dir());
        services
            .subscriptions
            .register("agent-a".to_owned(), vec![WhiteboardScope { topics: vec![] }])
            .await;

        let response = handle_ack_whiteboard(&services, "agent-a", 7, 1).await;
        assert_eq!(
            response.result,
            Some(IpcResult::AckWhiteboard { end_gate_seq: 7 }),
            "the reply echoes the acknowledged coordinate"
        );
        let subscription = load_whiteboard_subscription(&pool, "agent-a")
            .await
            .expect("row query")
            .expect("cursor row materialized on first ack");
        assert_eq!(subscription.cursor_gate_seq, 7);
    }

    #[tokio::test]
    async fn publish_event_marks_subscribed_peers_dirty_for_flush() {
        let (_pool_dir, pool) = test_pool().await;
        let services = services(pool.clone(), std::env::temp_dir());
        services.subscriptions.register("peer".to_owned(), decision_scope()).await;
        services.subscriptions.register("other".to_owned(), decision_scope()).await;

        // `handle_publish_event` appends and wakes subscribers; `peer` and
        // `other` are at cursor 0 so both become flush candidates.
        let event = new_event("agent-a", WhiteboardKind::Decision, "peer-visible");
        handle_publish_event(&services, "agent-a", event, 2, &CancellationToken::new()).await;
        let candidates = services.subscriptions.flush_candidates().await;
        assert!(candidates.contains(&"peer".to_owned()));
        assert!(candidates.contains(&"other".to_owned()));
    }

    // ---------------------------------------------------------------------
    // ADR-60 D6 minimal consolidation (Phase 4): the write path feeds the
    // trigger; the pass runs OUT-OF-BAND (the reply returns first) and lands
    // a `Consolidation` bookmark once the append threshold is crossed.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn publish_events_trigger_an_out_of_band_consolidation_pass() {
        use crate::consolidation::{Consolidator, CONSOLIDATION_TRIGGER_APPENDS};
        use concerto_memory::vector_store::SqliteVectorStore;

        let dir = tempfile::tempdir().expect("tempdir created");
        let (_pool_dir, pool) = test_pool().await;
        let store = std::sync::Arc::new(
            SqliteVectorStore::new(pool.clone()).await.expect("projection store opens"),
        );
        let mut services = services(pool.clone(), dir.path().to_path_buf());
        services.consolidation = Some(std::sync::Arc::new(Consolidator::new(
            pool.clone(),
            store,
            ProjectId("proj-d6-supervisor".to_owned()),
        )));

        // Fewer than the threshold: no pass may run.
        for n in 0..(CONSOLIDATION_TRIGGER_APPENDS - 1) {
            let event = new_event("agent-a", WhiteboardKind::Decision, &format!("note-{n}"));
            let response =
                handle_publish_event(&services, "agent-a", event, n, &CancellationToken::new())
                    .await;
            assert!(response.error.is_none());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !all_events(&pool)
                .await
                .iter()
                .any(|event| event.kind == WhiteboardKind::Consolidation),
            "no bookmark before the append threshold"
        );

        // The threshold append detaches the pass; the reply itself must not
        // wait for it (out-of-band), so we only poll for the eventual result.
        let event = new_event("agent-a", WhiteboardKind::Decision, "threshold-crosser");
        let response = handle_publish_event(
            &services,
            "agent-a",
            event,
            CONSOLIDATION_TRIGGER_APPENDS,
            &CancellationToken::new(),
        )
        .await;
        assert!(response.error.is_none(), "the reply is immediate");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if all_events(&pool)
                .await
                .iter()
                .any(|event| event.kind == WhiteboardKind::Consolidation)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the detached consolidation pass never recorded its bookmark"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
