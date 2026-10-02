# Native shell and security settings

Concerto's desktop console and `concerto --cli shell` use the Rust command
runtime. The agent's `shell` tool takes a program plus an argument array and
launches it directly. No Bash, PowerShell, or cmd profile is required.

## Commands

```text
help
ls .
cat README.md
write notes.txt 'a literal string'
cp notes.txt copy.txt
mv copy.txt renamed.txt
rm renamed.txt
run cargo test
run --cwd crates/core --timeout 60 cargo test
```

Quotes group arguments. There is no variable expansion, glob expansion, pipe,
redirection, or command substitution. Single quotes preserve Windows paths
exactly, including UNC paths and trailing backslashes. Agents should pass
structured arguments, for example `{"command":"cargo","args":["test"]}`.
The native tool rejects unknown fields, including caller-supplied security,
environment additions, and legacy shell-wrapping options.

File commands use Concerto's policy-gated filesystem tool. Interactive file
changes take effect after approval. `run` captures stdout/stderr and reports
the exit code; it does not provide a PTY, interactive stdin, or live output
streaming. Cancellation ends the current operation. Restarting the desktop
console cancels its current operation and captures the latest settings.

The one-shot CLI accepts structured operands without reparsing them:

```text
concerto --cli shell exec ls .
concerto --cli shell exec run cargo test
```

## Human-owned configuration

Desktop Settings → Shell provides Read only, Ask before changes, and Isolated
offline presets plus a JSON editor for every setting. Review shows the
permission changes before Confirm saves them. Reload saved security discards
the editor draft and loads the latest revision.

The CLI provides the same validated, revision-checked editing path:

```text
concerto --cli shell security show
concerto --cli shell security validate proposed-security.json
concerto --cli shell security apply proposed-security.json
```

Start from `security show` output, edit the JSON, then validate/apply it. Apply
prints the changed fields and requires typing `apply`. Revision is managed by
Concerto. Agent tools do not expose a security-settings mutation operation.

`[shell_security]` is loaded from the user-global config. Project config and
`CONCERTO_*` overrides cannot widen it or select a different compatibility
profile. Saves use an exclusive config lock and atomic file replacement.
Generic settings saves reject changes to security and reject stale security
snapshots. Existing agent runs retain their captured settings; stop/restart
those runs to revoke their existing access. The desktop console is cancelled
when a security reload changes its settings.

## Controls and guarantees

| Setting | Enforcement and limits |
|---|---|
| `processes`, `writes` | Deny/ask/policy ceilings apply in addition to existing policy rules, including allow-all rules. Process launches require approval when either processes or potential writes are set to ask. |
| `interpreter_compatibility` | Explicit permission for known command interpreters and host shebang scripts; no implicit shell wrapping. This is not a detector for arbitrary programs capable of interpreting code. |
| `allowed_executables` | Canonical absolute host paths. The selected executable is resolved before approval. Empty means normal policy/approval decides. Host allowlists are refused for container images because their identities differ. |
| `environment_allowlist` | Only named host variables are inherited. Loader-injection variables are rejected. No extra environment values can be supplied in a tool request. Image-defined environment remains image-defined. |
| `timeout_secs` | Wall-clock deadline; a command cannot raise the configured ceiling. |
| `max_output_bytes` | Capture ceiling per stdout/stderr stream; excess output is drained without growing the capture buffer. |
| `max_argument_bytes` | Bound on executable/argument bytes before spawn. |
| `protected_paths` | Project-relative paths protected from filesystem-tool reads/writes, including copy/move destinations and writes to an ancestor. The global config is also protected in the normal runtime. Metadata-only built-ins can still list names. |
| `history_enabled` | Controls in-memory command-result history. Native file commands do not record content in that history. Execution/approval audit remains enabled. |
| `network = "offline"` | Requires Docker/Podman isolation with networking disabled. Host mode refuses it. No destination/port allowlist is implemented yet. |
| `memory_bytes`, `max_processes`, `cpu_seconds` | Require the container backend. Memory and PID limits are container limits; CPU seconds uses a per-process inherited `ulimit`, not an aggregate CPU-time allowance. Host mode refuses these requirements. |
| `writes = "deny"` with processes enabled | Requires a read-only project mount in container mode. Host processes are refused because their writes cannot be confined. |

Container mode requires a locally installed Docker or Podman runtime and a
preloaded `container_image`; Concerto never pulls an image implicitly. It drops
capabilities, disallows new privileges, clears the image entrypoint, uses a
read-only image filesystem and bounded temporary storage, and removes its
named container after execution or cancellation. Windows container routing is
unsupported and is refused. No weaker backend is selected automatically.

Host mode uses the user's ambient OS permissions. Approved executables and
their descendants can access host files and networks and can replace a binary
at an approved path. Protected paths constrain native filesystem tools, not
arbitrary host programs or writable container project mounts. External-tool
writes bypass VirtualFs review. Cancellation kills ordinary Unix process
groups; it is not confinement against descendants that escape the group.
Automatic native test validation uses the same execution gate. Coverage helpers without a governed implementation refuse execution; run coverage explicitly through `run` or the shell tool.

Windows currently terminates the direct child and lacks an enforced Job Object
boundary. This implementation does not claim a complete OS sandbox.

Platform kernel confinement, destination-level networking, isolated workspace
diff import, PTYs, streaming, background tasks, pipelines/workflows, and secret
redaction/injection are remaining extensions. The native-shell CI workflow
runs core/config/tools/shell tests on Linux, Windows, and macOS; a workflow
definition is not proof that those platform runs have already passed.
