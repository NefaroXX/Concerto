# ADR-28: Shell Profiles and Integrated Toolchain

> **Archived** — superseded by [ADR-30](../ADR-30.md) (unified shell
> selection) and [ADR-29](../ADR-29.md) (AI-native shell runtime); the
> surviving profile/config/policy-facts decisions are consolidated into the
> ADR-30 rewrite (consolidated 2026-08-22). See
> [docs/adrs/README.md](../README.md) for the current index. Retained verbatim
> as the historical record; not active guidance.

**Status:** Superseded by ADR-30 (shell selection; surviving profile and policy-facts decisions consolidated there)

## Context

Concerto launches a shell in exactly two hardcoded ways:

1. **Interactive Terminal** (`crates/desktop/src/views/terminal.rs`) reads `$SHELL` → `/bin/sh` (Unix) or `%COMSPEC%` → `cmd.exe` (Windows). There is no profile concept, no config hook, and no way to inject a controlled `PATH`, environment, or startup script into the PTY.
2. **Agent shell tool** (`crates/tools/src/shell.rs`) uses `detect_os_default_shell()` and builds `ShellConfig` programmatically. The `allowlist` is empty by default (deny-by-default), `bypass_shell` is `false`, and the executable is not configurable per project or per context.

The policy engine (`crates/core/src/policy.rs`) can only reason about a command as a **raw string** (`Condition::CommandPattern`). It has no structured facts for the resolved executable, parsed `argv`, working directory, network intent, or destructive classification. The audit log (`crates/sessions/src/audit.rs`) records only the verdict and a hash of the input — never what *actually* ran (executable, argv, cwd, exit code, duration).

This ADR defined the shell-profile architecture. It deliberately **does not**
bundle any managed runtime; a managed Bash runtime remains a gated later slice
requiring a licensing/provenance review first.

## Decision

### 1. Backend abstraction (mirror the provider factory)

Introduce a `ShellBackend` trait and a `ShellProfileFactory`, mirroring `crates/providers/src/factory.rs` (`ProviderFactory` / `LlmProvider`).

```text
ShellBackend (trait)
    SystemProfile      -> runs a configured OS executable
    ManagedBash        -> Concerto-managed runtime (later slice)
    FutureCustomBackend

ShellProfileFactory
    profile_id(config) -> stable id
    build(config)      -> Arc<dyn ShellBackend>
    build_all(settings) -> HashMap<id, Arc<dyn ShellBackend>>
    resolve_for_context(settings, binding) -> Arc<dyn ShellBackend>
```

Initial placement: `crates/tools/src/shell_backend.rs`. `SystemProfile` is implemented; `ManagedBash` and `FutureCustomBackend` are explicit stubs that return a clear "not yet available" error so the wiring exists end-to-end without bundling a runtime.

### 2. Configuration model (layered config)

```text
AppConfig.shell_settings: ShellSettings

ShellSettings {
    profiles: Vec<ShellProfileConfig>,
    selected_profile: profile_id,        // unified by ADR-30
    managed: Option<ManagedEnvConfig>,   // later slice
}

ShellProfileConfig {
    id, name,
    backend: ShellBackendType,           // System | Managed | Custom
    executable: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,          // explicit env additions
    path_additions: Vec<PathBuf>,        // prepended to PATH
    startup_script: Option<PathBuf>,
    login: bool, interactive: bool,
    encoding: String,                    // default "utf-8"
    default_working_dir: WorkingDirBehavior,
    status: AvailabilityStatus,          // Available | Unavailable(reason)
}
```

ADR-30 replaced the original independent default bindings with one canonical
selection. Agent execution is the primary consumer; validation and the
integrated terminal deliberately follow it.

`default_shell_profiles()` returns shells detected on the current host. Settings
shows those profiles directly and provides one **Add profile** action for a
custom executable or environment. It does not create placeholder profiles for
shells that may not be installed.

A `Test profile` action reports executable path, shell version, working directory, `PATH`, and detected tools **without** mutating system configuration.

`ManagedEnvConfig` fields (`install_dir`, `runtime_manifest`, `tool_manifest`, `integrity`, `offline`, `version`, plus a manifest import/export type) live behind `Option` and are unused until the managed-runtime slices.

### 3. Schema migration

`SCHEMA_VERSION` goes `3 -> 4`. The new `shell_settings` field is added with a default; existing configs that lack it merge with `default_shell_profiles()` (following the `migration.rs` layered-merge pattern — non-breaking). No destructive migration.

### 4. Terminal page integration

`crates/desktop/src/views/terminal.rs`:
- Replace `selected_shell()` with the canonical selected profile lookup.
- Pass `env`, `path_additions`, `startup_script`, `login/interactive`, and `encoding` into `iced_term::Settings` / `BackendSettings` when spawning.
- Apply saved profile changes by restarting the PTY. Profile selection lives in
  Settings so the terminal cannot drift from agent execution.
- Terminal widget spike resolved: `iced_term 0.8` supports `BackendSettings { program, args, env, working_directory }` plumbed through to `alacritty_terminal::tty::Options` → `portable-pty`'s `CommandBuilder`; it is the only maintained option compatible with Iced 0.14.

### 5. Shell tool integration

`crates/tools/src/shell.rs`:
- `ShellConfig` is loaded from the canonical selected profile instead of `detect_os_default_shell()`.
- Agent execution routes through `ShellBackend`, keeping the existing `ToolExecutor` → `SimplePolicyEngine` gate.
- `cwd` defaults to the **active project root** unless an allowed working directory is explicitly supplied (existing `canonicalize_within` sandbox is preserved).
- A user-typed terminal command is never treated as an agent-authorized action — the terminal and the agent shell tool share *configuration* but never *process state*.
- Non-zero exits, wrong arguments, absent commands, and retryable process errors are **recoverable diagnostics**, not session-terminating events.

### 6. Policy integration (structured facts)

Extend `PolicyAction` (`crates/core/src/policy.rs` + `crates/core/src/types.rs`) with structured command facts:

```text
shell_profile_id: Option<String>,
resolved_executable: PathBuf,
argv: Vec<String>,
working_directory: PathBuf,
network_requested: bool,
filesystem_scope: FilesystemScope,
destructive_classification: DestructiveClass,
```

Add `Condition` variants `ResolvedExecutable`, `ArgvPattern`, `WorkingDir`, and `ShellProfile`. The sandbox-profile check stays enforced first; a profile may additionally declare a sandbox intent (so "this profile is read-only" is expressible). Aliases, shell functions, scripts, symlinks, and wrapper tools must resolve to auditable execution information (the resolved executable + argv) before policy evaluation — a managed environment must not become a policy bypass.

### 7. Audit log expansion

`crates/sessions/src/audit.rs` + `AuditEntry`: add `profile_id`, `resolved_executable`, `argv`, `working_directory`, `exit_code`, `duration_ms`, and `toolchain_version`. New SQLite migration; existing rows default these to `NULL`/empty. Tool output, exit status, timeout, and resolved executable are recorded in the Tool Log.

### 8. Managed toolchain & custom tools (later slices)

- **Slice 2 — Managed Bash PoC**: one platform first (Linux). Lives in Concerto's app-data dir, never touches global `PATH`/registry/user shell config/project files. Controlled `PATH`, PTY-backed terminal, UTF-8, offline operation, versioned runtime + tool manifest, integrity verification.
- **Slice 3 — Cross-platform packaging**: Windows distribution approach evaluated and license-reviewed before implementation.
- **Slice 4 — Custom tool manager**: declarative manifests; custom tools can never silently bypass policy evaluation.

## Consequences

**Positive**
- Shell selection becomes config-driven and platform-explicit, not silently env-derived.
- Agent and interactive terminal share one profile model but separate processes/sessions.
- Policy can reason about *what actually ran*, not just a command string.
- Audit trail gains executable/argv/cwd/exit-code, enabling real post-hoc analysis.
- Mirrors the proven provider-factory pattern.

**Negative / cost**
- Config schema grows; v3→v4 migration must be maintained.
- New `ShellBackend` trait + factory is additional surface area.
- Structured policy facts + audit expansion touch core crates used widely; must stay non-breaking and warning-clean under `RUSTFLAGS="-D warnings"`.

**Risks / open questions**
- **Windows managed runtime**: MSYS2-derived vs BusyBox decision deferred to a licensing/provenance review.
- **Managed shell + deny-by-default allowlist**: managed commands need a profile-scoped allowlist so they are not silently blocked.
- **Per-profile credentials** (e.g. SSH keys for managed `ssh`/`scp`) reuse `CredentialStore`.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](../README.md)).*
