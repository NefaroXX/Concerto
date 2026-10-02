//! Argv-direct execution for the Concerto native shell (ADR-76).
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use concerto_core::shell_security::{ShellIsolation, ShellNetwork, ShellPermission, ShellSecurity};
use concerto_core::traits::{PolicyEngine, Tool};
use concerto_core::types::{
    CapabilitySet, CommandPolicyFacts, CommandRouting, DestructiveClass, FilesystemScope,
    SandboxProfile, SessionContext, ToolOutput,
};
use concerto_core::{CancellationToken, ToolError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::container::{containerize, ContainerConfig};
use crate::process::ProcessHandle;
use crate::shell::ShellPlan;

/// Settings are captured at session construction and never accepted in tool input.
pub struct NativeProcessTool {
    security: ShellSecurity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}

struct Launch {
    program: String,
    args: Vec<String>,
    cwd: Utf8PathBuf,
    env: HashMap<String, String>,
    timeout: Duration,
    container: bool,
}

impl NativeProcessTool {
    pub fn new(security: ShellSecurity) -> Self {
        Self { security }
    }

    fn plan(&self, input: &Value, session: &SessionContext) -> Result<Launch, ToolError> {
        self.security.validate().map_err(invalid)?;
        let input: Input =
            serde_json::from_value(input.clone()).map_err(|e| invalid(e.to_string()))?;
        if self.security.processes == ShellPermission::Deny {
            return Err(denied("shell_security_processes"));
        }
        if input.command.is_empty()
            || input.command.contains('\0')
            || input.args.iter().any(|a| a.contains('\0'))
        {
            return Err(invalid("command and arguments must be nonempty/NUL-free"));
        }
        let bytes = input.args.iter().try_fold(input.command.len(), |n, a| {
            n.checked_add(a.len()).and_then(|n| n.checked_add(1))
        });
        if bytes.is_none_or(|n| n > self.security.max_argument_bytes) {
            return Err(denied("shell_argument_limit"));
        }
        let timeout = input.timeout_secs.unwrap_or(self.security.timeout_secs);
        if timeout == 0 || timeout > self.security.timeout_secs {
            return Err(denied("shell_timeout_limit"));
        }
        let root = session.project_dir.canonicalize().map_err(|e| invalid(e.to_string()))?;
        let cwd = input.cwd.as_ref().map_or_else(|| root.clone(), |p| root.join(p));
        let cwd = cwd.canonicalize().map_err(|e| invalid(e.to_string()))?;
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err(denied("shell_cwd_outside_project"));
        }
        let cwd = Utf8PathBuf::from_path_buf(cwd).map_err(|_| invalid("cwd must be UTF-8"))?;
        let container = self.security.isolation == ShellIsolation::Container;
        if !container
            && (self.security.network == ShellNetwork::Offline
                || self.security.memory_bytes.is_some()
                || self.security.max_processes.is_some()
                || self.security.cpu_seconds.is_some()
                || self.security.writes == ShellPermission::Deny)
        {
            return Err(denied("shell_host_cannot_enforce_requested_boundary"));
        }
        if !self.security.interpreter_compatibility && is_interpreter(Path::new(&input.command)) {
            return Err(denied("shell_interpreter_disabled"));
        }
        let env = self
            .security
            .environment_allowlist
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.clone(), value)))
            .collect();
        let mut launch = Launch {
            program: input.command,
            args: input.args,
            cwd,
            env,
            timeout: Duration::from_secs(timeout),
            container,
        };
        if container {
            // Host file identities cannot authorize a different executable in an image.
            if !self.security.allowed_executables.is_empty() {
                return Err(denied("shell_container_host_allowlist_unsupported"));
            }
            let mut config = ContainerConfig::new(&self.security.container_image);
            config.network = self.security.network == ShellNetwork::Unrestricted;
            let root =
                Utf8PathBuf::from_path_buf(root).map_err(|_| invalid("project must be UTF-8"))?;
            let plan = containerize(
                &ShellPlan::Direct { program: launch.program, args: launch.args },
                &config,
                &launch.cwd,
                &root,
            )?;
            let ShellPlan::Direct { program, mut args } = plan else {
                return Err(invalid("container plan must be direct"));
            };
            let mut limits = vec![
                "--pull=never".into(),
                "--entrypoint=".into(),
                "--cap-drop=ALL".into(),
                "--security-opt=no-new-privileges".into(),
                "--read-only".into(),
                "--tmpfs=/tmp:rw,nosuid,nodev,size=67108864".into(),
            ];
            if let Some(memory) = self.security.memory_bytes {
                limits.push(format!("--memory={memory}"));
            }
            if let Some(pids) = self.security.max_processes {
                limits.push(format!("--pids-limit={pids}"));
            }
            if let Some(cpu) = self.security.cpu_seconds {
                limits.push(format!("--ulimit=cpu={cpu}:{cpu}"));
            }
            #[cfg(unix)]
            limits.push(format!("--user={}:{}", nix::unistd::getuid(), nix::unistd::getgid()));
            if self.security.writes == ShellPermission::Deny {
                let mount = format!("{root}:{root}");
                for arg in &mut args {
                    if *arg == mount {
                        *arg = format!("{mount}:ro");
                    }
                }
            }
            args.splice(1..1, limits);
            launch.program =
                resolve_executable(&program, Path::new("/"))?.to_string_lossy().into_owned();
            launch.args = args;
        } else {
            let executable = resolve_executable(&launch.program, launch.cwd.as_std_path())?;
            if !self.security.interpreter_compatibility
                && (is_interpreter(&executable) || is_script(&executable)?)
            {
                return Err(denied("shell_interpreter_disabled"));
            }
            if !self.security.allowed_executables.is_empty()
                && !self
                    .security
                    .allowed_executables
                    .iter()
                    .any(|p| p.canonicalize().is_ok_and(|p| p == executable))
            {
                return Err(denied("shell_executable_not_allowed"));
            }
            launch.program = executable.to_string_lossy().into_owned();
        }
        Ok(launch)
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed { message: message.into() }
}
fn denied(rule: &str) -> ToolError {
    ToolError::PolicyDenied { rule: rule.into() }
}

fn is_interpreter(path: &Path) -> bool {
    let name = path.file_stem().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    matches!(
        name.as_str(),
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "cmd"
            | "powershell"
            | "pwsh"
            | "wscript"
            | "cscript"
    ) || path.extension().is_some_and(|e| {
        matches!(e.to_string_lossy().to_ascii_lowercase().as_str(), "bat" | "cmd" | "ps1")
    })
}

fn is_script(path: &Path) -> Result<bool, ToolError> {
    use std::io::Read;
    let mut header = [0; 2];
    let mut file = std::fs::File::open(path).map_err(|e| invalid(e.to_string()))?;
    let count = file.read(&mut header).map_err(|e| invalid(e.to_string()))?;
    Ok(count == 2 && header == *b"#!")
}

fn resolve_executable(program: &str, cwd: &Path) -> Result<PathBuf, ToolError> {
    let requested = Path::new(program);
    let candidates: Vec<PathBuf> = if requested.is_absolute() || requested.components().count() > 1
    {
        vec![cwd.join(requested)]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .filter(|p| p.is_absolute())
                    .flat_map(|p| {
                        let direct = p.join(program);
                        #[cfg(windows)]
                        {
                            vec![direct.clone(), p.join(format!("{program}.exe"))]
                        }
                        #[cfg(not(windows))]
                        {
                            vec![direct]
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates
        .into_iter()
        .find_map(|p| p.canonicalize().ok().filter(|p| p.is_file()))
        .ok_or_else(|| invalid(format!("executable not found: {program}")))
}

#[async_trait]
impl Tool for NativeProcessTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run an executable directly. command is a program path/name; args is an argv array. No shell expansion, pipelines or redirection. Security is controlled by the human client."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{"command":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1}},"required":["command"]})
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default().with_requirement("shell")
    }
    fn prepare_input(&self, input: &mut Value, session: &SessionContext) -> Result<(), ToolError> {
        let launch = self.plan(input, session)?;
        if !launch.container {
            input["command"] = json!(launch.program);
        }
        input["cwd"] = json!(launch.cwd);
        Ok(())
    }
    fn sandbox_profile(&self) -> Option<SandboxProfile> {
        (self.security.isolation == ShellIsolation::Container)
            .then_some(SandboxProfile::Containerized)
    }
    fn command_facts(&self, input: &Value, session: &SessionContext) -> Option<CommandPolicyFacts> {
        let launch = self.plan(input, session).ok()?;
        Some(CommandPolicyFacts {
            shell_profile_id: Some("concerto-native".into()),
            resolved_executable: Some(launch.program.clone().into()),
            argv: std::iter::once(launch.program).chain(launch.args).collect(),
            working_directory: Some(launch.cwd.into()),
            network_requested: self.security.network == ShellNetwork::Unrestricted,
            filesystem_scope: FilesystemScope::Anywhere,
            destructive_classification: DestructiveClass::classify_command(&input.to_string()),
            container_routing: if launch.container {
                CommandRouting::Containerized
            } else {
                CommandRouting::Direct
            },
        })
    }
    async fn execute(
        &self,
        input: Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let launch = self.plan(&input, session)?;
        // A named container is removed even when its client process is cancelled.
        let name = format!(
            "concerto-{}",
            concerto_core::ids::Ulid::new().to_string().to_ascii_lowercase()
        );
        let mut args = launch.args;
        if launch.container {
            args.splice(1..1, ["--name".to_owned(), name.clone()]);
        }
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let result = ProcessHandle::run_native(
            &launch.program,
            &refs,
            &launch.cwd,
            &launch.env,
            launch.timeout,
            self.security.max_output_bytes,
            cancel,
        )
        .await;
        if launch.container {
            let _ = ProcessHandle::run_native(
                &launch.program,
                &["rm", "--force", &name],
                Utf8Path::new("/"),
                &launch.env,
                Duration::from_secs(10),
                4096,
                CancellationToken::new(),
            )
            .await;
        }
        let output = result?;
        Ok(ToolOutput {
            summary: format!("Process exited with code {}", output.exit_code),
            data: json!({"exit_code":output.exit_code,"stdout":output.stdout,"stderr":output.stderr}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(root: &Path) -> SessionContext {
        SessionContext::new(concerto_core::ids::Ulid::new(), root.to_path_buf())
    }
    // verifies: unsupported isolation and interpreter settings refuse execution before spawn.
    #[test]
    fn guards_fail_closed() {
        let dir = tempfile::tempdir().expect("dir");
        let mut security = ShellSecurity::default();
        let tool = NativeProcessTool::new(security.clone());
        assert!(tool
            .plan(&json!({"command":"sh","args":["-c","echo unsafe"]}), &session(dir.path()))
            .is_err());
        security.network = ShellNetwork::Offline;
        let tool = NativeProcessTool::new(security);
        assert!(tool.plan(&json!({"command":"anything"}), &session(dir.path())).is_err());
    }
    // verifies: executable grants, cwd traversal, argument caps, and caller-supplied security are enforced.
    #[test]
    fn request_cannot_widen_security() {
        let dir = tempfile::tempdir().expect("dir");
        let executable = std::env::current_exe().expect("exe");
        let security = ShellSecurity {
            allowed_executables: vec![executable.clone()],
            ..ShellSecurity::default()
        };
        let tool = NativeProcessTool::new(security);
        let input = json!({"command":executable});
        assert!(tool.plan(&input, &session(dir.path())).is_ok());
        assert!(tool
            .plan(&json!({"command":executable,"cwd":".."}), &session(dir.path()))
            .is_err());
        assert!(tool
            .plan(&json!({"command":executable,"network":"unrestricted"}), &session(dir.path()))
            .is_err());
        assert!(tool
            .plan(&json!({"command":executable,"timeout_secs":86400}), &session(dir.path()))
            .is_err());
    }

    // verifies: approvals target the resolved executable even if a project symlink is changed later.
    #[cfg(unix)]
    #[test]
    fn prepared_request_freezes_executable_identity() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().expect("project");
        let approved = std::env::current_exe().expect("exe").canonicalize().expect("canonical");
        let alias = directory.path().join("tool");
        symlink(&approved, &alias).expect("link");
        let tool = NativeProcessTool::new(ShellSecurity::default());
        let session = session(directory.path());
        let mut prepared = json!({"command":"./tool"});
        tool.prepare_input(&mut prepared, &session).expect("prepare");
        std::fs::remove_file(&alias).expect("unlink");
        symlink("/bin/sh", &alias).expect("retarget");
        let launch = tool.plan(&prepared, &session).expect("approved plan");
        assert_eq!(Path::new(&launch.program), approved);
    }
}
