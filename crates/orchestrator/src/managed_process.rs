//! H09 — managed process lifecycle adapter: stop/recreate for
//! Concerto-owned processes only, with readiness observation.
//!
//! A *managed* process is one Concerto itself started through
//! [`ManagedProcessRegistry::register_and_spawn`], whose executable,
//! arguments, working directory, and permitted environment are recorded in a
//! [`ManagedProcessSpec`] at spawn time. Stop and recreate operate ONLY on
//! registry entries — the registry holds the live [`tokio::process::Child`]
//! handles it spawned, so there is no PID/address input by which a caller
//! could aim the adapter at a process Concerto does not own. An unknown id is
//! [`ManagedProcessError::NotFound`]; a known id under a different owner is
//! [`ManagedProcessError::OwnershipRefused`]. Both refuse BEFORE any signal
//! is sent.
//!
//! # What recreate is (and is not)
//!
//! Recreate = stop the recorded child (SIGKILL semantics via
//! [`tokio::process::Child::kill`], then reaped) + spawn the recorded spec
//! again + observe readiness. **Exact live process-memory restoration is
//! UNSUPPORTED** ([`MEMORY_STATE_UNSUPPORTED`]): a restarted process starts
//! with fresh memory; in-memory state is never captured and never restored.
//! Every [`RecreateReport`] carries `memory_state_restored: false` so the
//! type itself refuses to claim otherwise — a restart is never labeled memory
//! recovery.
//!
//! # Honest limits
//!
//! * Restart only; memory state never (see above).
//! * Stop signals the DIRECT child only. Grandchildren a managed process
//!   spawned itself are outside the stop claim (no process-group kill — the
//!   adapter never signals a PID it did not spawn).
//! * Readiness is observed, never assumed: [`ReadinessProbe::Alive`] requires
//!   the child to still be running after a bounded grace window; a
//!   fast-exiting process fails readiness loudly
//!   ([`ManagedProcessError::NotReady`]) instead of being reported
//!   "recreated". [`ReadinessProbe::Command`] additionally requires an
//!   exit-zero probe command within its timeout.
//! * Recreate replays the RECORDED spec exactly: callers cannot smuggle new
//!   arguments, environment, or cwd into a recreate (re-register to change
//!   configuration — the registry never merges caller config over recorded
//!   config).
//! * Stdio of managed children is null: output capture is out of scope (the
//!   shell/process tools own output capture; this adapter owns lifecycle).
//!
//! # Runtime authorship (ADR-65)
//!
//! The agent supplies only identities (spec fields, agent ids, probes); every
//! registry transition is performed by these runtime functions.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use concerto_core::checkpoint::{EffectCoverage, EffectKind, EffectSupport};
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Bounds and declared limits
// ---------------------------------------------------------------------------

/// Upper bound on recorded arguments per spec.
pub const MANAGED_PROCESS_MAX_ARGS: usize = 64;
/// Upper bound on one recorded argument, in bytes.
pub const MANAGED_PROCESS_MAX_ARG_BYTES: usize = 4096;
/// Upper bound on recorded environment entries per spec.
pub const MANAGED_PROCESS_MAX_ENV_VARS: usize = 64;
/// Upper bound on one environment key, in bytes.
pub const MANAGED_PROCESS_MAX_ENV_KEY_BYTES: usize = 256;
/// Upper bound on one environment value, in bytes.
pub const MANAGED_PROCESS_MAX_ENV_VALUE_BYTES: usize = 4096;
/// Upper bound on the executable string, in bytes.
pub const MANAGED_PROCESS_MAX_EXECUTABLE_BYTES: usize = 4096;
/// Upper bound on a command-probe timeout, in seconds.
pub const MANAGED_PROCESS_MAX_PROBE_TIMEOUT_SECS: u64 = 60;
/// How long `stop` waits for the child to exit after kill before giving up.
pub const MANAGED_PROCESS_STOP_WAIT: Duration = Duration::from_secs(5);
/// Grace window for the [`ReadinessProbe::Alive`] observation.
pub const MANAGED_PROCESS_ALIVE_GRACE: Duration = Duration::from_secs(1);

/// The explicit unsupported claim: exact live process-memory restoration has
/// no backend. Also used as the `Qualified` limitation in
/// [`process_adapter_coverage`], so the preflight always surfaces it.
pub const MEMORY_STATE_UNSUPPORTED: &str =
    "exact live process-memory restoration is unsupported: stop/recreate restarts the process from its recorded executable/arguments/permitted config; in-memory state is never captured and never restored (a restart is not recovery of memory state)";

/// How recreate restores the process (operator-readable, also used as the
/// `Qualified.method` string in [`process_adapter_coverage`]).
pub const MANAGED_PROCESS_RECREATE_METHOD: &str =
    "stop + recreate Concerto-owned processes from the recorded executable/arguments/permitted config, with readiness observation after recreate";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the managed-process adapter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManagedProcessError {
    /// Unknown process id (never a guess about outside processes).
    #[error("managed process not found: {0}")]
    NotFound(String),
    /// The id is already registered.
    #[error("managed process already registered: {0}")]
    AlreadyExists(String),
    /// A foreign-owned process id aborted the operation before any signal.
    #[error("managed process {id} owned by {owner}: refusing")]
    OwnershipRefused {
        /// The contested process id.
        id: String,
        /// The holding agent.
        owner: String,
    },
    /// The spec is invalid (empty executable/id/owner, over bounds, …).
    #[error("invalid managed process spec: {0}")]
    Invalid(String),
    /// Spawn failed (the OS verdict, surfaced — never retried blindly).
    #[error("managed process spawn failed for {id}: {message}")]
    SpawnFailed {
        /// The process id.
        id: String,
        /// The OS failure.
        message: String,
    },
    /// Readiness was not observed after recreate (fail-closed: the child was
    /// still recreated — the report states that — but the operation errors
    /// instead of claiming a ready process).
    #[error("managed process {id} not ready after recreate: {detail}")]
    NotReady {
        /// The process id.
        id: String,
        /// What the probe observed.
        detail: String,
    },
    /// Stop failed (kill/reap error).
    #[error("managed process stop failed for {id}: {message}")]
    StopFailed {
        /// The process id.
        id: String,
        /// The failure.
        message: String,
    },
    /// The operation was cancelled.
    #[error("managed process operation cancelled")]
    Cancelled,
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ManagedProcessError> {
    if cancel.is_cancelled() {
        Err(ManagedProcessError::Cancelled)
    } else {
        Ok(())
    }
}

/// Wait out one child's reap with a bound, honoring cancellation.
///
/// Shared by [`ManagedProcessRegistry::stop`] and the stop phase of
/// [`ManagedProcessRegistry::recreate`]: both remove the registry entry
/// unconditionally once the reap begins (see the call sites), so this reports
/// the honest outcome — `Ok` when reaped, `StopFailed` on timeout,
/// `Cancelled` — and never touches the registry itself.
///
/// `wait` is the child's `wait()` future, taken as a parameter so the
/// timeout/cancel arms stay unit-testable with a pending future: no test can
/// hold a real child past SIGKILL, so the timeout arm is otherwise
/// unreachable deterministically.
async fn reap_child(
    wait: impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
    bound: Duration,
    cancel: &CancellationToken,
    id: &str,
) -> Result<(), ManagedProcessError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ManagedProcessError::Cancelled),
        result = tokio::time::timeout(bound, wait) => match result {
            Ok(_) => Ok(()),
            Err(_) => Err(ManagedProcessError::StopFailed {
                id: id.to_string(),
                message: format!("child did not exit within {bound:?} after kill"),
            }),
        },
    }
}

// ---------------------------------------------------------------------------
// Spec, probes, reports
// ---------------------------------------------------------------------------

/// The recorded identity of a Concerto-owned process: executable, arguments,
/// working directory, and the permitted environment subset — captured at
/// spawn time, replayed verbatim at recreate time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProcessSpec {
    /// Process identity (ULID string, registry key).
    pub id: String,
    /// Program as spawned (verbatim — resolved by the OS at spawn).
    pub executable: String,
    /// Arguments as spawned.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory as spawned (absolute).
    pub cwd: PathBuf,
    /// Permitted environment subset as spawned (never merged with caller
    /// config at recreate).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Agent id that owns this process (stop/recreate require this agent).
    pub owner: String,
    /// Registration time, Unix epoch millis (UTC).
    pub registered_at_ms: i64,
}

impl ManagedProcessSpec {
    /// Validate and construct a spec (fail-fast: a bad spec is a caller bug,
    /// never a registered guess).
    pub fn new(
        id: String,
        executable: String,
        args: Vec<String>,
        cwd: PathBuf,
        env: BTreeMap<String, String>,
        owner: String,
        registered_at_ms: i64,
    ) -> Result<Self, ManagedProcessError> {
        if id.is_empty() {
            return Err(ManagedProcessError::Invalid("id must not be empty".to_string()));
        }
        if executable.is_empty() {
            return Err(ManagedProcessError::Invalid("executable must not be empty".to_string()));
        }
        if executable.len() > MANAGED_PROCESS_MAX_EXECUTABLE_BYTES {
            return Err(ManagedProcessError::Invalid(format!(
                "executable exceeds {MANAGED_PROCESS_MAX_EXECUTABLE_BYTES} bytes"
            )));
        }
        if owner.is_empty() {
            return Err(ManagedProcessError::Invalid(
                "owner must name the owning agent".to_string(),
            ));
        }
        if !cwd.is_absolute() {
            return Err(ManagedProcessError::Invalid("cwd must be absolute".to_string()));
        }
        if args.len() > MANAGED_PROCESS_MAX_ARGS {
            return Err(ManagedProcessError::Invalid(format!(
                "args hold {} entries, bound is {MANAGED_PROCESS_MAX_ARGS}",
                args.len()
            )));
        }
        for arg in &args {
            if arg.len() > MANAGED_PROCESS_MAX_ARG_BYTES {
                return Err(ManagedProcessError::Invalid(format!(
                    "argument exceeds {MANAGED_PROCESS_MAX_ARG_BYTES} bytes"
                )));
            }
        }
        if env.len() > MANAGED_PROCESS_MAX_ENV_VARS {
            return Err(ManagedProcessError::Invalid(format!(
                "env holds {} entries, bound is {MANAGED_PROCESS_MAX_ENV_VARS}",
                env.len()
            )));
        }
        for (key, value) in &env {
            if key.len() > MANAGED_PROCESS_MAX_ENV_KEY_BYTES {
                return Err(ManagedProcessError::Invalid(format!(
                    "env key exceeds {MANAGED_PROCESS_MAX_ENV_KEY_BYTES} bytes"
                )));
            }
            if value.len() > MANAGED_PROCESS_MAX_ENV_VALUE_BYTES {
                return Err(ManagedProcessError::Invalid(format!(
                    "env value exceeds {MANAGED_PROCESS_MAX_ENV_VALUE_BYTES} bytes"
                )));
            }
        }
        Ok(Self { id, executable, args, cwd, env, owner, registered_at_ms })
    }
}

/// How readiness is observed after (re)create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessProbe {
    /// The child is still running after the bounded grace window.
    Alive,
    /// Additionally, this probe command exits zero within its timeout.
    Command {
        /// Probe program.
        program: String,
        /// Probe arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Probe timeout, in seconds (capped at
        /// [`MANAGED_PROCESS_MAX_PROBE_TIMEOUT_SECS`]).
        timeout_secs: u64,
    },
}

/// What the readiness observation saw (returned by value — the durable
/// readiness record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessObservation {
    /// Which probe ran (`alive` or `command:<program>`).
    pub probe: String,
    /// Observation time, Unix epoch millis (UTC).
    pub observed_at_ms: i64,
    /// Whether readiness held.
    pub ready: bool,
    /// Operator-readable detail.
    pub detail: String,
}

/// What one `register_and_spawn` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnReport {
    /// The process id.
    pub id: String,
    /// The OS pid, when the spawn produced one.
    pub pid: Option<u32>,
    /// The readiness observation.
    pub readiness: ReadinessObservation,
}

/// What one `stop` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopReport {
    /// The process id.
    pub id: String,
    /// The OS pid at stop time, when known.
    pub pid: Option<u32>,
    /// Whether the child was still running when stopped.
    pub was_running: bool,
}

/// What one `recreate` did.
///
/// `memory_state_restored` is ALWAYS `false`: the type carries the
/// unsupported claim so no consumer can mistake a restart for memory
/// recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecreateReport {
    /// The process id.
    pub id: String,
    /// The OS pid before recreate, when known.
    pub old_pid: Option<u32>,
    /// The OS pid after recreate.
    pub new_pid: Option<u32>,
    /// The readiness observation for the new child.
    pub readiness: ReadinessObservation,
    /// Always `false` — see the module docs. A restart is not recovery of
    /// memory state.
    pub memory_state_restored: bool,
}

/// Unix epoch milliseconds (UTC).
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// This adapter's share of a preflight's `offered` list: `Process` is
/// `Qualified` (restart-only with readiness); every other kind is
/// `Unsupported`.
#[must_use]
pub fn process_adapter_coverage() -> Vec<EffectCoverage> {
    let mut offered = vec![EffectCoverage {
        kind: EffectKind::Process,
        support: EffectSupport::Qualified {
            method: MANAGED_PROCESS_RECREATE_METHOD.to_string(),
            limitation: MEMORY_STATE_UNSUPPORTED.to_string(),
        },
    }];
    for kind in [EffectKind::File, EffectKind::Database, EffectKind::Shell, EffectKind::Network] {
        offered.push(EffectCoverage {
            kind,
            support: EffectSupport::Unsupported {
                reason: format!(
                    "the managed-process adapter claims no {} effects (registry holds Concerto-spawned children only)",
                    kind.as_str()
                ),
            },
        });
    }
    offered
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct ManagedEntry {
    spec: ManagedProcessSpec,
    child: tokio::process::Child,
}

/// Registry of Concerto-owned processes: the only processes the adapter can
/// ever signal. Default-empty; entries are added by
/// [`ManagedProcessRegistry::register_and_spawn`] (spawned by Concerto) and
/// removed only when a spawn fails after a stop (the entry would otherwise
/// claim a child that no longer exists).
///
/// No `Debug` impl by design: entries hold live OS child handles, which have
/// no debug rendering worth logging (pids surface in reports instead).
#[derive(Default)]
pub struct ManagedProcessRegistry {
    inner: tokio::sync::Mutex<HashMap<String, ManagedEntry>>,
}

impl ManagedProcessRegistry {
    /// An empty registry (no process is owned until spawned through it).
    pub fn new() -> Self {
        Self { inner: tokio::sync::Mutex::new(HashMap::new()) }
    }

    /// The owning agent of `id`, if registered (the oracle read surface).
    pub async fn owner_of(&self, id: &str) -> Option<String> {
        self.inner.lock().await.get(id).map(|entry| entry.spec.owner.clone())
    }

    /// Whether `id` is registered.
    pub async fn contains(&self, id: &str) -> bool {
        self.inner.lock().await.contains_key(id)
    }

    /// Spawn `spec` and record it as Concerto-owned, then observe readiness.
    ///
    /// Refuses duplicates ([`ManagedProcessError::AlreadyExists`]) before
    /// spawning — re-registering an id can never shadow a live child.
    pub async fn register_and_spawn(
        &self,
        spec: ManagedProcessSpec,
        probe: ReadinessProbe,
        cancel: &CancellationToken,
    ) -> Result<SpawnReport, ManagedProcessError> {
        check_cancel(cancel)?;
        let mut guard = self.inner.lock().await;
        if guard.contains_key(&spec.id) {
            return Err(ManagedProcessError::AlreadyExists(spec.id.clone()));
        }
        let mut child = spawn_spec(&spec)
            .map_err(|message| ManagedProcessError::SpawnFailed { id: spec.id.clone(), message })?;
        let pid = child.id();
        let readiness = observe_readiness(&mut child, &probe, cancel).await;
        if !readiness.ready {
            // A child that never became ready is still ours (and still
            // running or exited): keep the entry so `stop` can reap it, but
            // report loudly — readiness is observed, never assumed.
            let id = spec.id.clone();
            guard.insert(id.clone(), ManagedEntry { spec, child });
            return Err(ManagedProcessError::NotReady { id, detail: readiness.detail.clone() });
        }
        let id = spec.id.clone();
        guard.insert(id.clone(), ManagedEntry { spec, child });
        Ok(SpawnReport { id, pid, readiness })
    }

    /// Stop a managed process. Refuses unknown ids and foreign owners BEFORE
    /// any signal. Reaps the child (no zombies).
    pub async fn stop(
        &self,
        id: &str,
        by_agent: &str,
        cancel: &CancellationToken,
    ) -> Result<StopReport, ManagedProcessError> {
        check_cancel(cancel)?;
        let mut guard = self.inner.lock().await;
        let entry =
            guard.get_mut(id).ok_or_else(|| ManagedProcessError::NotFound(id.to_string()))?;
        if entry.spec.owner != by_agent {
            return Err(ManagedProcessError::OwnershipRefused {
                id: id.to_string(),
                owner: entry.spec.owner.clone(),
            });
        }
        let pid = entry.child.id();
        let was_running = entry
            .child
            .try_wait()
            .map_err(|error| ManagedProcessError::StopFailed {
                id: id.to_string(),
                message: error.to_string(),
            })?
            .is_none();
        if was_running {
            entry.child.kill().await.map_err(|error| ManagedProcessError::StopFailed {
                id: id.to_string(),
                message: error.to_string(),
            })?;
        }
        // Reap with a bound (and honor cancellation while waiting): the
        // child must not linger as a zombie, but the wait must not park
        // forever either. The entry is removed unconditionally once the reap
        // begins — a started stop never retains an unreaped handle (the
        // timeout/cancel errors below still report honestly; only the
        // registry entry is gone, so the id is reusable).
        let wait = entry.child.wait();
        let outcome = reap_child(wait, MANAGED_PROCESS_STOP_WAIT, cancel, id).await;
        guard.remove(id);
        outcome?;
        Ok(StopReport { id: id.to_string(), pid, was_running })
    }

    /// Recreate a managed process: stop the recorded child, spawn the
    /// RECORDED spec verbatim (no caller config merged), observe readiness.
    ///
    /// Refuses unknown ids and foreign owners before any signal. On success
    /// the report's `memory_state_restored` is `false` by construction.
    pub async fn recreate(
        &self,
        id: &str,
        by_agent: &str,
        probe: ReadinessProbe,
        cancel: &CancellationToken,
    ) -> Result<RecreateReport, ManagedProcessError> {
        check_cancel(cancel)?;
        // Ownership + existence resolve BEFORE any signal (fail-closed).
        let spec = {
            let guard = self.inner.lock().await;
            let entry =
                guard.get(id).ok_or_else(|| ManagedProcessError::NotFound(id.to_string()))?;
            if entry.spec.owner != by_agent {
                return Err(ManagedProcessError::OwnershipRefused {
                    id: id.to_string(),
                    owner: entry.spec.owner.clone(),
                });
            }
            entry.spec.clone()
        };
        // Stop the old child (best-effort kill errors still reap: a child
        // that already exited stops exactly — absence is a valid stop).
        let old_pid = {
            let mut guard = self.inner.lock().await;
            let entry =
                guard.get_mut(id).ok_or_else(|| ManagedProcessError::NotFound(id.to_string()))?;
            let pid = entry.child.id();
            let running = entry.child.try_wait().map(|status| status.is_none()).unwrap_or(false);
            if running {
                // A kill failure here aborts the recreate: the old child may
                // still hold the port/file the new one needs, and proceeding
                // would pile a second child over the first.
                entry.child.kill().await.map_err(|error| ManagedProcessError::StopFailed {
                    id: id.to_string(),
                    message: error.to_string(),
                })?;
            }
            let wait = entry.child.wait();
            // Unconditional-on-attempt like `stop` above: once the old child's
            // reap begins the entry goes regardless of the outcome, so a
            // timed-out or cancelled recreate never retains an unreaped
            // handle (the error still reports honestly below).
            let outcome = reap_child(wait, MANAGED_PROCESS_STOP_WAIT, cancel, id).await;
            guard.remove(id);
            outcome?;
            pid
        };
        check_cancel(cancel)?;
        // Spawn the RECORDED spec — never caller-supplied config.
        let mut child = spawn_spec(&spec)
            .map_err(|message| ManagedProcessError::SpawnFailed { id: id.to_string(), message })?;
        let new_pid = child.id();
        let readiness = observe_readiness(&mut child, &probe, cancel).await;
        {
            let mut guard = self.inner.lock().await;
            guard.insert(id.to_string(), ManagedEntry { spec, child });
        }
        if !readiness.ready {
            return Err(ManagedProcessError::NotReady {
                id: id.to_string(),
                detail: readiness.detail.clone(),
            });
        }
        Ok(RecreateReport {
            id: id.to_string(),
            old_pid,
            new_pid,
            readiness,
            memory_state_restored: false,
        })
    }
}

fn spawn_spec(spec: &ManagedProcessSpec) -> Result<tokio::process::Child, String> {
    let mut command = tokio::process::Command::new(&spec.executable);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command.spawn().map_err(|error| error.to_string())
}

/// Observe readiness for `child` (borrowed — the caller retains registry
/// ownership): `Alive` polls `try_wait` over the bounded grace window;
/// `Command` runs the probe program with its timeout. Cancellation aborts
/// the observation (the child keeps running under the registry — an aborted
/// observation is never a kill).
async fn observe_readiness(
    child: &mut tokio::process::Child,
    probe: &ReadinessProbe,
    cancel: &CancellationToken,
) -> ReadinessObservation {
    let observed_at_ms = now_millis();
    match probe {
        ReadinessProbe::Alive => {
            // The child must STILL be running when the grace window expires:
            // poll until the deadline; an exit at any point fails readiness
            // loudly instead of reporting a dead child "recreated".
            let deadline = tokio::time::Instant::now() + MANAGED_PROCESS_ALIVE_GRACE;
            loop {
                if cancel.is_cancelled() {
                    return ReadinessObservation {
                        probe: "alive".to_string(),
                        observed_at_ms,
                        ready: false,
                        detail: "readiness observation cancelled".to_string(),
                    };
                }
                match child.try_wait() {
                    Ok(None) => {
                        if tokio::time::Instant::now() >= deadline {
                            return ReadinessObservation {
                                probe: "alive".to_string(),
                                observed_at_ms,
                                ready: true,
                                detail: "child still running through the grace window".to_string(),
                            };
                        }
                    }
                    Ok(Some(status)) => {
                        return ReadinessObservation {
                            probe: "alive".to_string(),
                            observed_at_ms,
                            ready: false,
                            detail: format!("child exited before readiness held: {status}"),
                        };
                    }
                    Err(error) => {
                        return ReadinessObservation {
                            probe: "alive".to_string(),
                            observed_at_ms,
                            ready: false,
                            detail: format!("readiness poll failed: {error}"),
                        };
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        ReadinessProbe::Command { program, args, timeout_secs } => {
            let timeout_secs = (*timeout_secs).clamp(1, MANAGED_PROCESS_MAX_PROBE_TIMEOUT_SECS);
            let probe_name = format!("command:{program}");
            // The managed child must still be alive for a probe to mean
            // anything: a probe passing against a dead child proves nothing.
            match child.try_wait() {
                Ok(None) => {}
                _ => {
                    return ReadinessObservation {
                        probe: probe_name,
                        observed_at_ms,
                        ready: false,
                        detail: "managed child already exited before the probe ran".to_string(),
                    };
                }
            }
            let mut command = tokio::process::Command::new(program);
            command
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let run = async {
                match command.spawn() {
                    Ok(mut probe_child) => {
                        let waited = tokio::time::timeout(
                            Duration::from_secs(timeout_secs),
                            probe_child.wait(),
                        )
                        .await;
                        match waited {
                            Ok(Ok(status)) if status.success() => {
                                (true, "probe exited zero".to_string())
                            }
                            Ok(Ok(status)) => (false, format!("probe exited non-zero: {status}")),
                            Ok(Err(error)) => (false, format!("probe wait failed: {error}")),
                            Err(_) => {
                                let _ = probe_child.kill().await;
                                (false, format!("probe timed out after {timeout_secs}s"))
                            }
                        }
                    }
                    Err(error) => (false, format!("probe spawn failed: {error}")),
                }
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => ReadinessObservation {
                    probe: probe_name,
                    observed_at_ms,
                    ready: false,
                    detail: "readiness observation cancelled".to_string(),
                },
                (ready, detail) = run => ReadinessObservation {
                    probe: probe_name,
                    observed_at_ms,
                    ready,
                    detail,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec(id: &str, owner: &str) -> ManagedProcessSpec {
        let dir = std::env::temp_dir();
        ManagedProcessSpec::new(
            id.to_string(),
            "sleep".to_string(),
            vec!["30".to_string()],
            dir,
            BTreeMap::new(),
            owner.to_string(),
            1_700_000_000_000,
        )
        .expect("valid spec")
    }

    #[test]
    fn spec_validation_is_fail_fast() {
        let dir = std::env::temp_dir();
        assert!(ManagedProcessSpec::new(
            String::new(),
            "sleep".to_string(),
            Vec::new(),
            dir.clone(),
            BTreeMap::new(),
            "agent-a".to_string(),
            0
        )
        .is_err());
        assert!(ManagedProcessSpec::new(
            "id".to_string(),
            String::new(),
            Vec::new(),
            dir.clone(),
            BTreeMap::new(),
            "agent-a".to_string(),
            0
        )
        .is_err());
        assert!(ManagedProcessSpec::new(
            "id".to_string(),
            "sleep".to_string(),
            Vec::new(),
            dir.clone(),
            BTreeMap::new(),
            String::new(),
            0
        )
        .is_err());
        assert!(ManagedProcessSpec::new(
            "id".to_string(),
            "sleep".to_string(),
            Vec::new(),
            PathBuf::from("relative/path"),
            BTreeMap::new(),
            "agent-a".to_string(),
            0
        )
        .is_err());
        let too_many = vec!["x".to_string(); MANAGED_PROCESS_MAX_ARGS + 1];
        assert!(ManagedProcessSpec::new(
            "id".to_string(),
            "sleep".to_string(),
            too_many,
            dir,
            BTreeMap::new(),
            "agent-a".to_string(),
            0
        )
        .is_err());
    }

    #[test]
    fn adapter_coverage_is_restart_only_never_memory_recovery() {
        let offered = process_adapter_coverage();
        let process =
            offered.iter().find(|entry| entry.kind == EffectKind::Process).expect("process");
        match &process.support {
            EffectSupport::Qualified { limitation, .. } => {
                assert!(limitation.contains("memory"), "{limitation}");
            }
            other => panic!("process must be qualified restart-only, got {other:?}"),
        }
        assert!(MEMORY_STATE_UNSUPPORTED.contains("unsupported"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_recreate_round_trips_with_readiness() {
        let registry = ManagedProcessRegistry::new();
        let cancel = CancellationToken::new();
        let spawn = registry
            .register_and_spawn(test_spec("p1", "agent-a"), ReadinessProbe::Alive, &cancel)
            .await
            .expect("spawn");
        assert!(spawn.pid.is_some());
        assert!(spawn.readiness.ready);

        let recreated = registry
            .recreate("p1", "agent-a", ReadinessProbe::Alive, &cancel)
            .await
            .expect("recreate");
        assert!(!recreated.memory_state_restored, "a restart never restores memory state");
        assert!(recreated.readiness.ready);
        assert!(recreated.new_pid.is_some());
        assert_ne!(recreated.old_pid, recreated.new_pid, "recreate spawns a NEW process");

        let stop = registry.stop("p1", "agent-a", &cancel).await.expect("stop");
        assert!(stop.was_running);
        assert!(!registry.contains("p1").await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreign_owner_is_refused_and_process_survives() {
        let registry = ManagedProcessRegistry::new();
        let cancel = CancellationToken::new();
        registry
            .register_and_spawn(test_spec("p1", "agent-a"), ReadinessProbe::Alive, &cancel)
            .await
            .expect("spawn");
        // Stop by a foreign agent refuses BEFORE any signal.
        match registry.stop("p1", "agent-b", &cancel).await {
            Err(ManagedProcessError::OwnershipRefused { id, owner }) => {
                assert_eq!(id, "p1");
                assert_eq!(owner, "agent-a");
            }
            other => panic!("foreign stop must refuse, got {other:?}"),
        }
        // Recreate by a foreign agent refuses too — and the child survives
        // both refusals (nothing was signaled).
        match registry.recreate("p1", "agent-b", ReadinessProbe::Alive, &cancel).await {
            Err(ManagedProcessError::OwnershipRefused { .. }) => {}
            other => panic!("foreign recreate must refuse, got {other:?}"),
        }
        assert!(registry.contains("p1").await, "refusals must not disturb the child");
        let stop = registry.stop("p1", "agent-a", &cancel).await.expect("owner stop");
        assert!(stop.was_running, "the child survived both refusals");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unknown_ids_never_touch_outside_processes() {
        let registry = ManagedProcessRegistry::new();
        let cancel = CancellationToken::new();
        assert!(matches!(
            registry.stop("nope", "agent-a", &cancel).await,
            Err(ManagedProcessError::NotFound(_))
        ));
        assert!(matches!(
            registry.recreate("nope", "agent-a", ReadinessProbe::Alive, &cancel).await,
            Err(ManagedProcessError::NotFound(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fast_exiting_process_fails_readiness_loudly() {
        let registry = ManagedProcessRegistry::new();
        let cancel = CancellationToken::new();
        let dir = std::env::temp_dir();
        let spec = ManagedProcessSpec::new(
            "short".to_string(),
            "true".to_string(),
            Vec::new(),
            dir,
            BTreeMap::new(),
            "agent-a".to_string(),
            0,
        )
        .expect("valid spec");
        match registry.register_and_spawn(spec, ReadinessProbe::Alive, &cancel).await {
            Err(ManagedProcessError::NotReady { id, .. }) => assert_eq!(id, "short"),
            other => panic!("a fast-exiting child must fail readiness, got {other:?}"),
        }
        // The entry is kept so the (exited) child can be reaped via stop.
        let stop = registry.stop("short", "agent-a", &cancel).await.expect("reap");
        assert!(!stop.was_running);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_probe_requires_exit_zero() {
        let registry = ManagedProcessRegistry::new();
        let cancel = CancellationToken::new();
        registry
            .register_and_spawn(test_spec("p1", "agent-a"), ReadinessProbe::Alive, &cancel)
            .await
            .expect("spawn");
        // A passing probe recreates fine.
        let probe = ReadinessProbe::Command {
            program: "true".to_string(),
            args: Vec::new(),
            timeout_secs: 5,
        };
        let recreated =
            registry.recreate("p1", "agent-a", probe, &cancel).await.expect("probe recreate");
        assert!(recreated.readiness.ready);
        // A failing probe refuses loudly — the child was still recreated
        // (the error says so), but readiness is not claimed.
        let probe = ReadinessProbe::Command {
            program: "false".to_string(),
            args: Vec::new(),
            timeout_secs: 5,
        };
        match registry.recreate("p1", "agent-a", probe, &cancel).await {
            Err(ManagedProcessError::NotReady { .. }) => {}
            other => panic!("failing probe must refuse readiness, got {other:?}"),
        }
        registry.stop("p1", "agent-a", &cancel).await.expect("cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_honors_cancellation() {
        let registry = ManagedProcessRegistry::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            registry
                .register_and_spawn(test_spec("p1", "agent-a"), ReadinessProbe::Alive, &cancel)
                .await,
            Err(ManagedProcessError::Cancelled)
        ));
        assert!(!registry.contains("p1").await);
    }

    #[tokio::test]
    async fn reap_timeout_reports_stop_failed() {
        // Failure injection for the reap bound: a wait that never resolves
        // (no real child can survive SIGKILL, so the timeout arm is
        // otherwise unreachable deterministically).
        let pending = std::future::pending::<std::io::Result<std::process::ExitStatus>>();
        match reap_child(pending, Duration::from_millis(10), &CancellationToken::new(), "p1").await
        {
            Err(ManagedProcessError::StopFailed { id, message }) => {
                assert_eq!(id, "p1");
                assert!(message.contains("did not exit within"), "{message}");
            }
            other => panic!("a reap that never completes must time out, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reap_honors_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let pending = std::future::pending::<std::io::Result<std::process::ExitStatus>>();
        assert!(matches!(
            reap_child(pending, Duration::from_secs(5), &cancel, "p1").await,
            Err(ManagedProcessError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_attempt_cancel_keeps_the_entry_for_retry() {
        // Boundary pin: cancellation BEFORE any signal is not an attempt —
        // the entry is retained so the owner can retry. Removal is
        // unconditional only once the reap begins (see `stop`/`recreate`).
        let registry = ManagedProcessRegistry::new();
        let live = CancellationToken::new();
        registry
            .register_and_spawn(test_spec("p1", "agent-a"), ReadinessProbe::Alive, &live)
            .await
            .expect("spawn");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            registry.stop("p1", "agent-a", &cancel).await,
            Err(ManagedProcessError::Cancelled)
        ));
        assert!(registry.contains("p1").await, "no signal sent: entry retained for retry");
        let stop = registry.stop("p1", "agent-a", &live).await.expect("owner stop retries");
        assert!(stop.was_running);
        assert!(!registry.contains("p1").await);
    }
}
