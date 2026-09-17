# Concerto security boundaries

Concerto executes model-generated actions on a real developer machine. Its
policy, virtual filesystem, shell validation, plugin limits, audit log, and undo
features reduce risk; they do **not** make generated code trusted or provide a
complete operating-system sandbox.

## API exposure

`concerto-api-server` exposes session, task, spend, and event-stream routes.

- Default bind: `127.0.0.1`, port 3000; configurable with
  `CONCERTO_API_HOST` and `CONCERTO_API_PORT`.
- Authentication: bearer API key from `CONCERTO_API_KEY`. A non-loopback bind
  is rejected at startup unless BOTH `CONCERTO_API_KEY` and a non-empty
  `CONCERTO_PROJECT_ROOTS` allowlist are configured.
- Session-root confinement: when `CONCERTO_PROJECT_ROOTS` (read directly by
  the server; config-file roots do not apply to it) is non-empty, `POST
  /v1/sessions` refuses any `project_dir` outside the canonicalized allowlist
  with HTTP 403. Paths are canonicalized before the component-safe prefix
  check, so symlinks and `..` cannot escape a configured root and `/srv/proj`
  does not admit `/srv/proj2`. An empty allowlist (the default) is permissive.
- Desktop consent gate: when `project_roots` is configured, opening or
  switching to an out-of-root project shows an Allow/Deny modal. Allow applies
  for the process lifetime only and is never persisted; Deny aborts the
  switch. The gate is local awareness only — the api-server is confined by
  the server-side rules above, not by this dialog.
- With a configured key, `/v1/health` is public and the other versioned API
  routes pass through bearer authentication. With no key on a loopback bind,
  the middleware permits requests to all versioned routes.
- OpenAPI/Swagger routes are disabled unless `CONCERTO_API_DOCS=1`. The Swagger
  UI route is mounted outside the auth layer; `/v1/openapi.json` is inside it
  and therefore requires the key when one is configured.
- Request bodies are limited to 1 MiB by default.
- Legacy redirects lead to versioned routes; do not assume a redirect bypasses
  authentication at the destination.

Binding beyond loopback exposes an automation service capable of reaching
project data and providers. Use network-layer access control and a strong,
unique API key; do not expose it directly to the public Internet.

The `project_roots` allowlist is defense-in-depth: it constrains session roots
but is not a security boundary against a caller who already controls the
api-server process or its environment, since such a caller can alter or unset
the allowlist before startup.

## Tool and policy boundary

Registered calls pass through `ToolExecutor`, which applies spend checks,
`SimplePolicyEngine`, approval, event/audit emission, and then the tool.

- Configured policy rules are first-match-wins.
- `SimplePolicyEngine` denies an unmatched action.
- The desktop explicitly creates an allow-all rule in expert/no-rules mode so
  an empty policy configuration does not disable all coding tools.
- Shell validation maintains an independent hard denylist that is consulted
  before allow-all mode.
- Approval is a user decision, not proof that a command is safe.
- **Read-dedupe cache never bypasses policy.** Identical plain filesystem reads
  may be served from `resource_facts` without re-executing, but only when the
  policy engine's advisory evaluation returns an explicit `Allow` for that read
  (ADR-65 F1a). `Deny`/`RequireApproval` verdicts fall through to the executor,
  where the normal policy and approval gates apply on every call. Served reads
  are separately audited as `ServedFromCache` (ADR-65 F1b), are scoped to the
  canonical project root and are never stale: every serve re-stats the disk and
  re-hashes the cached bytes, and any write/shell/git side effect dirties the
  row and purges its cached content, forcing fresh execution.

See [Policy Rules](docs/policy-rules.md) for exact condition values. Keep narrow
rules before catch-alls and test them on a disposable project.

## Shell process boundary

The shell tool launches the selected profile with the project working directory
unless configured otherwise. It applies command validation, timeout clamping,
stdout/stderr caps, cancellation, and child-process termination behavior.

- **Structured parsing**: When `bypass_shell: true`, commands are spawned
  directly with `args` as a vector — no shell interpretation. This is the
  only mode in which the `command` field is the unshelled executable name.
- **Shell-wrap quoting**: When `bypass_shell: false` (default), args are
  shell-quoted (POSIX single-quote escape on Unix, double-quote with
  backslash/`%` escape on Windows cmd) before being joined into the command
  string passed to the shell. An arg containing `;`, `|`, `` ` ``, `$()`,
  `>`, `<`, newline, or any other metacharacter is passed as a literal to
  the underlying command — it cannot terminate the command or inject a second
  one. **Validation splitting**: the pre-execution denylist is matched against
  the *raw (unquoted)* string so that patterns like `rm -rf /` catch injection
  attempts regardless of quoting; the allowlist is matched against the *shell-
  quoted* string so that anchored patterns like `^echo( .*)?$` continue to
  match when args contain quotes. The `FilesystemTool` root check rejects
  `cwd` values that escape the project root.
- **Windows caveat**: On Windows, shell-quoting in `bypass_shell: false`
  mode follows cmd's quoting rules, which are weaker than POSIX. Operators
  who write custom allowlist patterns for Windows should anchor carefully
  and prefer `bypass_shell: true` for commands that do not need shell
  features.

This is not a container. A permitted compiler, build script, interpreter, or
shell command can execute arbitrary behavior allowed by the user's OS account.
Command-pattern approval cannot inspect every transitive action a build tool
will perform. Keep important repositories backed up and review dependencies and
scripts before running them.

Profile executable paths and startup arguments are trusted configuration. A
malicious replacement executable at that path is outside Concerto's model-level
policy boundary.

## Filesystem boundary

`FilesystemTool` resolves agent paths under the selected project root and uses
`VirtualFs` to stage/review changes. Root/path validation rejects traversal and
escape attempts handled by its resolver. Disk reads can supply unchanged files;
writes/deletes are represented in the overlay and committed through the review
flow.

Limitations:

- Concerto runs with the user's filesystem permissions.
- OS ACLs, antivirus, synchronization tools, and external processes can still
  deny, modify, or race files.
- Virtual staging and Git undo are not an independent backup.
- Files changed by shell commands or external build tools may bypass the
  filesystem overlay even though the shell call itself was policy-gated.

## Credentials and providers

Provider secrets are stored through the OS keychain; configuration files contain
key names, not API-key values. Test-mode credential stores are environment
backed via `CredentialStore::from_env()` and should be limited to test
processes.

Prompts, selected project context, memory results, and tool schemas may be sent
to the configured remote provider. "Local-first" does not mean a cloud provider
receives no data. Ollama can provide a local path, subject to its configured API
base and model behavior.

Supported provider implementations are OpenAI, Anthropic, Google, OpenRouter,
Ollama, NVIDIA NIM, and OpenCode-compatible endpoints. OpenAI-compatible proxies
may differ in streaming/tool-call behavior; protocol normalization is tested but
cannot guarantee every proxy/model combination.

## Plugin boundary

WASM plugins execute in wasmtime with module-size checks, fuel/time/resource
limits, manifest capabilities, and host-function mediation. The host limits what
a guest can request through exported functions.

Current limits:

- This is not full OS-level isolation.
- Tool, provider, and memory-adapter plugins all execute through the same WASM
  host and are constrained by capability checks, wasmtime fuel/size/time limits,
  and host-function mediation. OS-level or container sandboxing is deferred.
- `SandboxProfile::Containerized` is not implemented.
- A vulnerability in a granted host function, runtime, or capability resolver
  can cross the intended boundary.

Only install plugins from sources you trust and grant the smallest scope.

## Memory, audit, and telemetry

Session, audit, spend, and memory data are persisted locally in SQLite-backed
stores. Project scoping is enforced in memory queries, but these databases may
contain source-derived text, prompts, model responses, paths, and tool metadata.
Protect them with normal OS account and disk-security controls.

Prometheus, OpenTelemetry, and Langfuse export are optional. Enabling an exporter
can transmit operational metadata to its configured endpoint. Review the
endpoint and retention policy before use.

Audit records improve traceability; they do not prove the absence of an action
outside instrumented paths.

## Failure and recovery boundary

Transient provider failures use configurable backoff with finite production
defaults for attempts, elapsed outage time, time to first byte, and stream idle
time. An explicitly migrated configuration may still set
`retry.max_elapsed_seconds = None`; operators doing so accept the loss of that
one fuse, while attempt and stream deadlines remain active. Recoverable
tool/specialist correction has bounded attempts. Partial changes are kept for
review when bounded recovery is exhausted. Cancellation is propagated to
provider and process work.

Recovery does not guarantee process-crash durability or exactly-once external
effects. Re-running a non-idempotent external command can duplicate its effects.
Only automatic retry behavior that is safe for the relevant operation should be
enabled.

## Reporting security issues

Do not post credentials, private source, exploit details against a live service,
or sensitive logs in a public issue. Provide the commit, platform, affected
boundary, reproduction against a disposable project, impact, and a sanitized
trace through the private channel described in [`SECURITY.md`](SECURITY.md).
