use camino::Utf8Path;
use concerto_core::ToolError;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Maximum bytes captured from stdout or stderr before truncation (10 MB).
const MAX_OUTPUT_SIZE: u64 = 10 * 1024 * 1024;

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
        let mut command = Command::new(cmd);
        command.args(args);
        command.current_dir(cwd.as_std_path());
        command.kill_on_drop(true);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        if let Some(env) = env {
            command.envs(env.iter());
        }
        Self::spawn_and_collect(command, timeout, cancel).await
    }

    async fn spawn_and_collect(
        mut command: Command,
        timeout: Duration,
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

        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                kill_process_group(&mut child);
                // Reap deterministically so the process cannot linger as a zombie.
                let _ = child.wait().await;
                Err(ToolError::Cancelled)
            }

            result = tokio::time::timeout(timeout, async {
                // Read stdout, stderr, and wait for exit **concurrently**
                // so pipe buffers never fill up while the child is alive.
                let (status_result, stdout, stderr) = tokio::join!(
                    child.wait(),
                    Self::read_with_limit(stdout),
                    Self::read_with_limit(stderr),
                );

                let exit_code = match status_result {
                    Ok(s) => s.code().unwrap_or(-1),
                    Err(_) => -1,
                };
                ProcessOutput { exit_code, stdout, stderr }
            }) => {
                match result {
                    Ok(output) => Ok(output),
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

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
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
