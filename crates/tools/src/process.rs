use crate::cpu_accounting::{self, CPU_SAMPLE_INTERVAL};
use camino::Utf8Path;
use concerto_core::ToolError;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Maximum bytes captured from stdout or stderr before truncation (10 MB).
const MAX_OUTPUT_SIZE: u64 = 10 * 1024 * 1024;

/// A CPU-time budget for a single spawn (threat-model §6, gap #6).
///
/// Budgets are opt-in: [`Self::from_secs`] returns `None` for `0`, and every
/// spawn that receives `None` behaves exactly as it did before budgets
/// existed (the wall-clock timeout is the only deadline). Enforcement is
/// layered — a process-group watchdog where [`cpu_accounting::supported`],
/// plus the `ulimit` backstop the shell tool puts in front of POSIX-wrapped
/// plans (see `ShellTool::with_cpu_limit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuBudget {
    /// Ceiling in seconds of accumulated user+system CPU time.
    seconds: u64,
}

impl CpuBudget {
    /// A budget of `seconds`, or `None` for `0` — an explicit zero means
    /// "off", never "kill immediately".
    pub fn from_secs(seconds: u64) -> Option<Self> {
        (seconds > 0).then_some(Self { seconds })
    }

    /// The ceiling in seconds.
    pub fn seconds(&self) -> u64 {
        self.seconds
    }

    /// The ceiling as a [`Duration`], for deadline comparisons.
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.seconds)
    }
}

/// Outcome of a completed process execution.
#[derive(Debug)]
pub struct ProcessOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Spawn and collect output from a child process with cancel and timeout.
pub struct ProcessHandle;

impl ProcessHandle {
    /// Spawns `cmd` with `args` and `cwd`, waits for completion with cancel
    /// and timeout support.  Returns `ProcessOutput`.
    ///
    /// # Deadlock prevention
    ///
    /// stdout and stderr are read **concurrently** with `child.wait()` via
    /// `tokio::join!` so that pipe buffers are drained while the process
    /// is still running, preventing a full-pipe deadlock.
    pub async fn run(
        cmd: &str,
        args: &[&str],
        cwd: &Utf8Path,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, ToolError> {
        Self::run_with_env(cmd, args, cwd, None, timeout, cancel).await
    }

    /// Like [`run`], but merges `env` over the inherited environment before
    /// spawning. Used by shell profiles that declare environment additions or
    /// `PATH` additions.
    pub async fn run_with_env(
        cmd: &str,
        args: &[&str],
        cwd: &Utf8Path,
        env: Option<&HashMap<String, String>>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, ToolError> {
        Self::run_limited(cmd, args, None, cwd, env, timeout, None, false, cancel).await
    }

    /// Like [`run_with_env`], but appends `raw_tail` to the child's command
    /// line *verbatim* — no CRT argument escaping — after `args`.
    ///
    /// Windows-only mechanism, and the load-bearing half of threat-model §6
    /// #3: `cmd.exe` must receive `/D /V:OFF /S /C "<operand>"` with the
    /// operand byte-for-byte intact, because `/S` strips exactly the first
    /// and last quote of the remainder. CRT-escaping that operand (what
    /// `Command::arg` does) corrupts cmd.exe's own quote stripping and
    /// re-opens the injection gap the escaping was meant to close.
    ///
    /// On non-Windows hosts there is no raw tail to preserve, so the operand
    /// is appended as an ordinary argument; no caller ever produces a
    /// verbatim plan off Windows (`effective_dialect_for` pins Unix to the
    /// POSIX dialect).
    pub async fn run_with_raw_tail(
        cmd: &str,
        args: &[&str],
        raw_tail: &str,
        cwd: &Utf8Path,
        env: Option<&HashMap<String, String>>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, ToolError> {
        Self::run_limited(cmd, args, Some(raw_tail), cwd, env, timeout, None, false, cancel).await
    }

    /// The single spawn entrypoint: cancel + wall-clock timeout + optional
    /// CPU-time budget (threat gap #6), with optional profile environment and
    /// optional verbatim `raw_tail` (see [`run_with_raw_tail`]).
    ///
    /// `cpu_budget` starts the process-group CPU watchdog; `kernel_backstop`
    /// states that the plan already carries the `ulimit`/`RLIMIT_CPU`
    /// backstop, so a `SIGXCPU` death is ours to report as a budget breach
    /// rather than an unexplained exit. Callers derive both from the same
    /// launch plan (see `ShellTool::plan_takes_cpu_backstop`), so an
    /// attribution can never disagree with what was actually applied.
    // Every parameter is an independent launch dimension (program, argv,
    // verbatim tail, cwd, env, deadline, cpu budget, backstop flag, cancel);
    // grouping them would not make any call site clearer.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_limited(
        cmd: &str,
        args: &[&str],
        raw_tail: Option<&str>,
        cwd: &Utf8Path,
        env: Option<&HashMap<String, String>>,
        timeout: Duration,
        cpu_budget: Option<CpuBudget>,
        kernel_backstop: bool,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, ToolError> {
        let mut command = Self::base_command(cmd, args, cwd, env);
        if let Some(tail) = raw_tail {
            #[cfg(windows)]
            command.raw_arg(tail);
            #[cfg(not(windows))]
            command.arg(tail);
        }
        Self::spawn_and_collect(command, timeout, cpu_budget, kernel_backstop, cancel).await
    }

    /// Common `Command` setup shared by every spawn entrypoint so piped
    /// stdio, `kill_on_drop`, cwd, and env merging cannot drift apart.
    fn base_command(
        cmd: &str,
        args: &[&str],
        cwd: &Utf8Path,
        env: Option<&HashMap<String, String>>,
    ) -> Command {
        let mut command = Command::new(cmd);
        command.args(args);
        command.current_dir(cwd.as_std_path());
        command.kill_on_drop(true);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        if let Some(env) = env {
            command.envs(env.iter());
        }
        command
    }

    async fn spawn_and_collect(
        mut command: Command,
        timeout: Duration,
        cpu_budget: Option<CpuBudget>,
        kernel_backstop: bool,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, ToolError> {
        #[cfg(unix)]
        {
            // Spawn in its own process group so a timeout/cancel can SIGKILL
            // the whole group (including grandchildren) with a single syscall.
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|e| ToolError::ExecutionFailed {
            message: format!("failed to spawn process: {e}"),
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // The child leads its own process group (unix, above), so its id is
        // also the group id the CPU watchdog samples. Off unix there is no
        // group of our own, and no accounting source either.
        #[cfg(unix)]
        let group_leader = child.id();
        #[cfg(not(unix))]
        let group_leader: Option<u32> = None;

        // Only Linux can sample group CPU time; fail open everywhere else
        // (the plan's `ulimit` backstop still applies where one was applied)
        // instead of arming a watchdog that can never fire.
        let watchdog_budget = if cpu_accounting::supported() {
            cpu_budget
        } else {
            if let Some(budget) = cpu_budget {
                tracing::warn!(
                    budget_secs = budget.seconds(),
                    "cpu budget configured but process-group cpu accounting is unsupported on \
                     this platform; only the shell ulimit backstop (where applied) can enforce it"
                );
            }
            None
        };

        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                kill_process_group(&mut child);
                // Reap deterministically so the process cannot linger as a zombie.
                let _ = child.wait().await;
                Err(ToolError::Cancelled)
            }

            // Parks forever unless a budget is configured and accounting is
            // available, so an unconfigured spawn is unchanged: cancel and
            // the wall-clock timeout are then the only live branches.
            used = cpu_watchdog(watchdog_budget, group_leader) => {
                kill_process_group(&mut child);
                // Reap deterministically so the process cannot linger as a zombie.
                let _ = child.wait().await;
                Err(cpu_breach_error(cpu_budget, used))
            }

            result = tokio::time::timeout(timeout, async {
                // Read stdout, stderr, and wait for exit **concurrently**
                // so pipe buffers never fill up while the child is alive.
                let (status_result, stdout, stderr) = tokio::join!(
                    child.wait(),
                    Self::read_with_limit(stdout),
                    Self::read_with_limit(stderr),
                );

                let status = match status_result {
                    Ok(status) => status,
                    // Keep the pre-existing `-1` exit code contract when the
                    // child's status cannot be collected.
                    Err(_) => return Ok(ProcessOutput { exit_code: -1, stdout, stderr }),
                };
                // Kernel-delivered budget kill: only when this plan carried
                // the `ulimit` backstop, so a program that merely *exits* with
                // this code is never misreported as a budget breach.
                if kernel_backstop && killed_by_cpu_limit(&status) {
                    if let Some(budget) = cpu_budget {
                        return Err(cpu_limit_error(budget));
                    }
                }
                let exit_code = status.code().unwrap_or(-1);
                Ok(ProcessOutput { exit_code, stdout, stderr })
            }) => {
                match result {
                    Ok(output) => output,
                    Err(_elapsed) => {
                        kill_process_group(&mut child);
                        // Reap deterministically so the process cannot linger as a zombie.
                        let _ = child.wait().await;
                        Err(ToolError::Timeout { timeout_secs: timeout.as_secs() })
                    }
                }
            }
        }
    }

    /// Read a pipe to completion, capping at MAX_OUTPUT_SIZE bytes.
    /// Once the cap is hit the rest of the pipe is drained and discarded
    /// to prevent resource exhaustion (spinning the loop without
    /// allocation).
    pub(crate) async fn read_with_limit<R: AsyncRead + Unpin>(reader: Option<R>) -> String {
        let Some(mut r) = reader else {
            return String::new();
        };
        let mut buf = [0u8; 8192];
        let mut output = Vec::new();
        let mut total: u64 = 0;
        let mut over_limit = false;
        loop {
            match r.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    total += n as u64;
                    if total > MAX_OUTPUT_SIZE {
                        if !over_limit {
                            // Capture up to the limit, then stop buffering.
                            let cap = n.saturating_sub((total - MAX_OUTPUT_SIZE) as usize);
                            output.extend_from_slice(&buf[..cap]);
                            over_limit = true;
                        }
                        // Drain the rest without allocating.
                        // Using a fixed-size loop to avoid unbounded read_to_end.
                        let _ = r.read(&mut buf).await;
                        continue;
                    }
                    output.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&output).to_string()
    }
}

/// SIGKILL the whole process group led by `child` and, as a belt-and-braces
/// fallback, the direct child itself.
///
/// The child is its own group leader (spawned via `process_group(0)` on unix),
/// so a negative-pid `kill` reaches every descendant it spawned, preventing
/// grandchildren from surviving a timeout/cancel. Shared with the git CLI
/// fallback.
pub(crate) fn kill_process_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        if let Some(pid) = child.id() {
            if pid > 0 {
                // Negative pid targets the whole process group; a direct
                // syscall so no subprocess spawn that could stall under load.
                let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
            }
        }
    }
    let _ = child.start_kill();
}

/// Poll `group`'s CPU time until it reaches `budget`, returning the reading
/// that crossed the line.
///
/// Parks forever when no budget is configured or when accounting is
/// unavailable, so the branch is inert and a no-budget spawn behaves exactly
/// as it did before budgets existed. An accounting failure deliberately does
/// **not** fall back to wall time — that is the timeout's job, and counting
/// wall time as CPU time would kill slow-but-idle commands.
async fn cpu_watchdog(budget: Option<CpuBudget>, group_leader: Option<u32>) -> Duration {
    let (Some(budget), Some(pgid)) = (budget, group_leader) else {
        return std::future::pending::<Duration>().await;
    };
    // Capture the group's lineage before the first sample: the leader's start
    // time lets later samples detect a recycled pgid and refuse to attribute a
    // foreign group's CPU. A leader whose stat line cannot be read means the
    // watchdog cannot verify reuse, so it stays disarmed (fail-open, matching
    // the unsupported-platform behaviour above).
    let Some(lineage) = cpu_accounting::capture_lineage(pgid) else {
        return std::future::pending::<Duration>().await;
    };
    let limit = budget.duration();
    loop {
        tokio::time::sleep(CPU_SAMPLE_INTERVAL).await;
        match cpu_accounting::group_cpu_time(&lineage) {
            Some(used) if used >= limit => return used,
            Some(_) => {}
            None => return std::future::pending::<Duration>().await,
        }
    }
}

/// Budget breach observed by the process-group watchdog.
fn cpu_breach_error(budget: Option<CpuBudget>, used: Duration) -> ToolError {
    // The watchdog parks forever without a budget, so `budget` is `Some` on
    // every path that reaches here; the fallback keeps the match total
    // (repo rule: no `unwrap`/`expect` in library code).
    let Some(budget) = budget else {
        return ToolError::ExecutionFailed {
            message: "cpu budget exceeded (budget unavailable)".into(),
        };
    };
    ToolError::ExecutionFailed {
        message: format!(
            "cpu budget exceeded: {used:?} of CPU time consumed by the process group \
             (budget {}s); group SIGKILLed",
            budget.seconds()
        ),
    }
}

/// Budget breach observed by the kernel's `RLIMIT_CPU` backstop — the
/// `ulimit` prelude the shell tool puts in front of POSIX-wrapped plans.
fn cpu_limit_error(budget: CpuBudget) -> ToolError {
    ToolError::ExecutionFailed {
        message: format!(
            "cpu budget exceeded: RLIMIT_CPU backstop reached at {}s (ulimit); command terminated",
            budget.seconds()
        ),
    }
}

/// True when `status` is an `RLIMIT_CPU` kill under a backstopped plan.
///
/// Two spellings, both Unix-only:
/// - the process that burned past the limit is killed directly (`SIGXCPU`,
///   which the kernel sends while the hard limit is still headroom), and
/// - a POSIX shell reports a child that died from a signal as exit code
///   `128 + signo`, which is what the wrapper around a backstopped command
///   returns.
///
/// Signal 24 is `SIGXCPU` on every supported Unix, resolved through `nix` so
/// the number is never hard-coded.
#[cfg(unix)]
fn killed_by_cpu_limit(status: &std::process::ExitStatus) -> bool {
    use nix::sys::signal::Signal;
    use std::os::unix::process::ExitStatusExt;

    let direct =
        status.signal().is_some_and(|signo| matches!(Signal::try_from(signo), Ok(Signal::SIGXCPU)));
    let shell_reported = status
        .code()
        .is_some_and(|code| matches!(Signal::try_from(code - 128), Ok(Signal::SIGXCPU)));
    direct || shell_reported
}

#[cfg(not(unix))]
fn killed_by_cpu_limit(_status: &std::process::ExitStatus) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use camino::Utf8PathBuf;
    #[cfg(unix)]
    use std::sync::Arc;

    #[cfg(unix)]
    #[tokio::test]
    async fn process_normal_exit() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let output =
            ProcessHandle::run("echo", &["hello"], root, Duration::from_secs(5), cancel.clone())
                .await
                .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("hello"), "stdout was: {:?}", output.stdout);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_cancellation() {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let cancel = CancellationToken::new();
        let handle_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            ProcessHandle::run("sleep", &["10"], &root, Duration::from_secs(30), handle_cancel)
                .await
        });
        cancel.cancel();
        let result = task.await.unwrap();
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::Cancelled => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let result =
            ProcessHandle::run("sleep", &["10"], root, Duration::from_millis(100), cancel).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::Timeout { .. } => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_large_output_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();

        let result = ProcessHandle::run(
            "sh",
            &["-c", "dd if=/dev/zero bs=1024 count=128 2>/dev/null"],
            root,
            Duration::from_secs(5),
            cancel,
        )
        .await;

        let output = result.expect("process with large output should complete without deadlock");
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.len(), 128 * 1024);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_stderr_captured() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let output = ProcessHandle::run(
            "sh",
            &["-c", "echo stderr_msg >&2"],
            root,
            Duration::from_secs(5),
            cancel,
        )
        .await
        .unwrap();
        assert!(output.stderr.contains("stderr_msg"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_custom_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let sub_dir = dir.path().join("subdir");
        std::fs::create_dir(&sub_dir).unwrap();
        let cancel = CancellationToken::new();
        let output =
            ProcessHandle::run("pwd", &[], root, Duration::from_secs(5), cancel).await.unwrap();
        assert!(
            output.stdout.trim().ends_with("subdir")
                || output.stdout.trim().ends_with(dir.path().to_str().unwrap())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_with_args_containing_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let output = ProcessHandle::run(
            "echo",
            &["hello world", "test"],
            root,
            Duration::from_secs(5),
            cancel,
        )
        .await
        .unwrap();
        assert!(output.stdout.contains("hello world"));
        assert_eq!(output.exit_code, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_nonexistent_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let result = ProcessHandle::run(
            "nonexistent_command_xyz123",
            &[],
            root,
            Duration::from_secs(5),
            cancel,
        )
        .await;
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_timeout_kills_process_group() {
        // A `sh -c` script that backgrounds a `sleep 30` grandchild, records
        // its pid, then blocks forever in `wait`. On timeout the whole process
        // group must be SIGKILLed so the grandchild does not outlive the call.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let result =
            ProcessHandle::run("sh", &["-c", &script], root, Duration::from_secs(2), cancel).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ToolError::Timeout { .. } => {}
            other => panic!("expected Timeout, got {other:?}"),
        }

        let pid_text = std::fs::read_to_string(&pid_file)
            .expect("grandchild should have written its pid before the wait");
        let pid: i32 = pid_text.trim().parse().expect("pid file must contain a number");
        assert!(pid > 0, "expected a positive pid, got {pid}");

        // The grandchild is SIGKILLed with the group and reparented to init,
        // which reaps it asynchronously, so poll briefly for it to disappear
        // instead of asserting instantly (a zombie would still report alive).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !process_is_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("background grandchild (pid {pid}) survived the timeout kill");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cpu_budget_breach_kills_a_spinning_child() {
        // A child that never yields burns CPU far past a one-second budget
        // long before the (deliberately generous) wall-clock timeout.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let result = ProcessHandle::run_limited(
            "sh",
            &["-c", "while :; do :; done"],
            None,
            root,
            None,
            Duration::from_secs(30),
            CpuBudget::from_secs(1),
            false,
            cancel,
        )
        .await;
        let error = result.expect_err("a spinning child must breach its cpu budget");
        let ToolError::ExecutionFailed { message } = error else {
            panic!("expected ExecutionFailed, got {error:?}");
        };
        assert!(message.contains("cpu budget exceeded"), "message was: {message}");
        assert!(message.contains("1s"), "the budget must be named: {message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_cpu_budget_leaves_spawn_behaviour_unchanged() {
        // Without a budget the watchdog parks forever: a CPU-burning child
        // still completes normally under the wall-clock timeout alone.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let cancel = CancellationToken::new();
        let output = ProcessHandle::run_limited(
            "sh",
            &["-c", "dd if=/dev/zero of=/dev/null bs=1M count=32 2>/dev/null"],
            None,
            root,
            None,
            Duration::from_secs(30),
            None,
            false,
            cancel,
        )
        .await
        .expect("no budget configured, so the child runs to completion");
        assert_eq!(output.exit_code, 0);
    }

    #[cfg(unix)]
    #[test]
    fn rslimit_cpu_death_is_recognized_only_under_a_backstop() {
        use std::os::unix::process::ExitStatusExt;

        // Direct SIGXCPU kill (signal 24), as the kernel delivers it.
        assert!(killed_by_cpu_limit(&std::process::ExitStatus::from_raw(24)));
        // A POSIX shell reports a signal-death child as 128 + signo.
        assert!(killed_by_cpu_limit(&std::process::ExitStatus::from_raw(152 << 8)));
        // Ordinary success/failure is never misread as a budget breach.
        assert!(!killed_by_cpu_limit(&std::process::ExitStatus::from_raw(0)));
        assert!(!killed_by_cpu_limit(&std::process::ExitStatus::from_raw(1 << 8)));
    }

    /// True if a process with `pid` still exists on this system.
    #[cfg(unix)]
    fn process_is_alive(pid: i32) -> bool {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        match kill(Pid::from_raw(pid), Signal::SIGTERM) {
            Ok(()) => true,
            Err(nix::errno::Errno::EPERM) => true, // exists, just not ours to signal
            Err(_) => false,                       // ESRCH or other -> gone
        }
    }
}
