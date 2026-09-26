# ADR-72: Containerized sandbox profile — OS-level isolation via a container runtime

**Status:** Proposed

Composes with ADR-62 (ToolExecutor + `VirtualFs`), ADR-28 §6 (structured command
facts), and ADR-55 §2 (shell containment + intent gating). Supersedes: none.
Addresses security-threat-model §6 gap #1 ("No Containerized Plugin Sandbox")
and DEFERRED row 36.

**Date:** 2026-09-26

**Deciders:** Concerto architecture + maintainer direction

## Context

`SandboxProfile` (`crates/core/src/types.rs`) declares four isolation levels but
enforces none. The policy engine rejects every non-`None` profile with the
single rule name `sandbox_profiles_not_implemented`
(`SimplePolicyEngine::check_sandbox`, `crates/core/src/policy.rs`), so the enum
is a stub: `None` runs with the invoking user's ambient authority, and no other
profile can ever be selected. Plugins are confined only by the WASM capability
sandbox; the shell runs with full user authority (modulo the ADR-55 containment
heuristic, the hardcoded shell denylist, and the row-45 CPU budget).

The trust boundary is model-authored code. A shell command or plugin chosen by
the model is untrusted input; the policy engine, approval sink, and containment
scan are in-process guards that share the process's authority. They reduce risk
but cannot contain a deliberately hostile command, because nothing sits between
the command and the host kernel. `Containerized` is the profile whose whole
purpose is to supply that missing kernel-level boundary.

Row 42 (argv-direct execution + cmd quoting) and row 45 (process-group CPU
watchdog + `ulimit` backstop) already shape how a command is launched. This ADR
must compose with them, not replace them.

## Decision

### 1. Profile semantics

| Profile | Meaning | Enforcement |
| --- | --- | --- |
| `None` (and `None` field) | No sandboxing; run as the invoking user. | Unchanged — current behavior, byte-identical. |
| `ReadOnlyFs` | Filesystem writes denied, reads allowed. | **Still not implemented.** Denied by policy (`sandbox_profiles_not_implemented`). Implementing it is a separate ADR (Landlock/`ro` bind mounts). |
| `NetworkIsolated` | Network egress denied. | **Still not implemented.** Denied by policy (`sandbox_profiles_not_implemented`). Plugin egress allowlisting (row 46) is adjacent but not this profile. |
| `Containerized` | The command is executed inside an OS-level container with its own filesystem/network/process view. | Implemented by this ADR: opt-in runtime detection + policy admission + container invocation routing. |

`None` semantics are frozen. Existing call sites all pass
`sandbox_profile: None`, so the `Containerized` path is unreachable until a
caller selects it — making the first slice behavior-preserving for every
current run.

### 2. Trust boundary and fail-closed matrix

The container runtime is the boundary. Everything inside the container is
untrusted; the host mounts into the container are the deliberate, enumerable
attack surface.

| Condition | Verdict | Rule name |
| --- | --- | --- |
| `None` / `Some(None)` | pass to normal rule evaluation | — |
| `ReadOnlyFs`, `NetworkIsolated` | `Deny` | `sandbox_profiles_not_implemented` |
| `Containerized`, no runtime binary found | `Deny` | `sandbox_containerized_runtime_unavailable` |
| `Containerized`, runtime found, action has no container-routable plan | `Deny` | `sandbox_containerized_unenforceable` |
| `Containerized`, runtime found, well-formed plan | pass to normal rule evaluation | — |

`Containerized` is **fail-closed**: absence of a detected runtime is an explicit
refusal, never a silent fallback to ambient authority. A configured-but-broken
runtime (binary on `PATH`, daemon down) also fails closed at spawn time: the
runtime's own non-zero exit is surfaced as a tool error, never masked into a
success. "Well-formed plan" is defined narrowly and conservatively: the action
must carry structured command facts for a container-routable tool (`shell`);
any other tool under `Containerized` is refused as unenforceable rather than
silently run unconfined.

### 3. Runtime detection

- Supported runtimes: **Docker** and **Podman** on Linux/macOS, in that
  preference order (`docker`, then `podman`). Windows-native container runtimes
  are out of scope for v1 (see §5).
- Detection is a **pure `PATH` probe**: resolve the binary name against `PATH`
  and require a regular, executable file. Detection does **not** start the
  binary, contact a daemon, or perform a `--version` handshake — that keeps
  detection deterministic, side-effect-free, and testable without a runtime
  present. Daemon availability is re-verified by the runtime itself at
  invocation; its failure is fail-closed (§2).
- The probe is a trait (`ContainerRuntimeProbe`) so the policy engine and the
  shell tool can be tested with injected found/absent/malformed results and do
  not need a container in CI.
- The system probe caches its result behind a refreshable cell
  (`SystemContainerRuntime::new()`), so repeated policy evaluations do not
  rescan `PATH`; `refresh()` invalidates the cache for a runtime installed
  mid-session.

### 4. Container invocation (composes with rows 42/45)

When a container-routable plan is executed under `Containerized`:

- The existing `ShellPlan` (row 42) is the **inner command**, wrapped as:
  `Direct { program, args }` → `[program, args…]`;
  `Wrapped { program, switches, operand }` → `[program, switches…, operand]`.
  The container runtime argv is spawned **argv-direct** (`ShellPlan::Direct`
  with `program = <runtime>`), so no host shell is introduced by the sandbox.
- The project root is bind-mounted read-write at its own absolute path, and the
  container working directory is the same effective `cwd`; a `cwd` that does not
  fall under the mounted project root is refused (fail-closed) rather than
  mounted implicitly.
- Network defaults to **none** (`--network none`) and is opt-in per config.
- Host environment is **not** forwarded wholesale; only explicitly configured
  `KEY=VALUE` entries are passed with `-e`. This avoids leaking provider keys
  and credentials (row 47) into model-driven processes.
- The row-45 CPU budget is **not duplicated**: the `ulimit` prelude built by
  `with_cpu_limit` is already embedded in a `Wrapped` operand and therefore
  rides inside the container command unchanged; the process-group watchdog
  still supervises the `docker`/`podman` process from the host. The container
  runtime is not asked to re-impose a CPU ceiling.
- The image is operator-supplied; no image is pulled implicitly. A missing
  image fails at the runtime, fail-closed.

### 5. Windows

Windows has no `/bin/sh` and no `docker run`-compatible shell assumption, and
the repository hard-denies `unsafe_code`, which blocks the `windows-sys` FFI
(`CreateJobObjectW`/`AssignProcessToJobObject`) needed for a first-class Job
Objects implementation. Therefore:

- **Decision (v1):** the `Containerized` profile is **not supported on Windows**.
  The runtime probe returns `Unavailable`, so the policy engine refuses
  `Containerized` with `sandbox_containerized_runtime_unavailable`. This is
  fail-closed and honest.
- **Residual (recorded):** a future Windows story could use Job Objects for
  process/memory limits, but kernel-level filesystem/network isolation on
  Windows would need either a container runtime (Docker Desktop / Windows
  Containers) with a Windows-compatible invocation shape, or a narrowly scoped,
  audited `unsafe` FFI dependency. Both require a superseding ADR. Neither is
  in scope here.

### 6. Migration and rollback

- No data or config format changes in this slice. Selection of the profile is
  caller-driven (the `PolicyAction::sandbox_profile` field); no config key is
  added, so no migration is required.
- Rollback is trivial: `None` is the default and every existing call site uses
  it, so reverting the enforcement change restores the prior stub behavior
  exactly. Because the container route is opt-in, disabling it is a code/config
  change at the call site, not a data migration.

## Consequences

- `SandboxProfile` becomes partially real: two variants remain explicit stubs,
  `Containerized` gains a detection+enforcement+invocation path.
- `SimplePolicyEngine` gains an injectable runtime probe; its default is the
  system probe. Existing behavior for `None` and the two stub profiles is
  unchanged.
- The shell tool gains an opt-in container configuration; with it unset the
  spawn path is byte-identical to today.
- A container runtime present on a host no longer causes silent confinement —
  it only permits a caller that explicitly selects `Containerized` to proceed.

## Verification

- Deterministic unit tests cover detection (found / absent / malformed),
  policy enforcement (allow-with-runtime, refuse-without, unenforceable plan,
  `None` unchanged, stub profiles still denied), named-rule audit rows, and
  container invocation construction (argv, env, mounts, network, inner-command
  composition with the row-42 plan and row-45 `ulimit` prelude) **at the seam**.
- No test or CI job requires a container runtime to be installed.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo deny check`.

## Residuals

- Full plumbing of a user-facing config key that selects `Containerized` for a
  session is not part of this ADR; selection is programmatic for now.
- `ReadOnlyFs` and `NetworkIsolated` remain stubs with their own ADRs.
- Windows kernel-level isolation is out of scope (§5).
- Plugin execution under `Containerized` (rather than the WASM capability
  sandbox) is out of scope; this ADR governs the shell/process path.

## Implementation status (2026-09-26)

**Status note only.** The Status (`Proposed`) and every decision clause above
are unchanged; this section records what actually landed and what the landed
slice does *not* yet guarantee. Read it alongside the Residuals.

### Landed, in three stacked commits

| Commit | Landed |
|---|---|
| `d00582b` | Design record (this ADR) + the enforceable core half: `core::sandbox` runtime detection (`ContainerRuntime` docker/podman, injectable `ContainerRuntimeProbe`, cached/refreshable `SystemContainerRuntime`, pure `PATH` probe) and the policy admission gate in `SimplePolicyEngine::check_sandbox`. |
| `3ae6ea5` | The execution half: `tools::container` builds the `docker`/`podman run` argv from an already-planned `ShellPlan` and returns it as `ShellPlan::Direct` (runtime spawned argv-direct, no host shell introduced); `ShellTool::with_container` + an injectable `with_container_runtime_probe` seam. Detection is fail-closed: no runtime or an unsupported platform yields an explicit `PolicyDenied` (`sandbox_containerized_runtime_unavailable` / `sandbox_containerized_unsupported_platform`), never a silent unconfined passthrough. Row-45's `ulimit` prelude rides inside the container unchanged and is not duplicated as a runtime CPU ceiling. `command_facts` routes through the same container plan, so the audited argv is the argv that runs. |
| `fdf4800` | The `CommandRouting` marker — see below. |

`None` remains byte-identical to pre-ADR-72 behavior in all three commits, and
no existing call site selects `Containerized`, so every current run is
unchanged. No test or CI job requires a container runtime.

### `CommandRouting`: the marker the policy engine requires (`fdf4800`)

`Containerized` admission needs a third condition beyond "runtime found" and
"plan is container-routable": the action's `CommandPolicyFacts` must assert
`CommandRouting::Containerized` (`crates/core/src/types.rs`). A working
directory proves nothing about routing — a shell invocation can carry a `cwd`
and still be launched unconfined on the host — so the marker is what makes the
claim checkable at the policy seam.

The engine enforces **both** directions, and both denials are named rules
recorded in the audit trail:

| Condition | Verdict | Rule |
|---|---|---|
| `Containerized` profile, runtime found, well-formed plan, **no** routing marker | `Deny` | `sandbox_containerized_routing_missing` |
| Routing marker asserted, profile is **not** `Containerized` (including `None`) | `Deny` | `sandbox_container_routing_profile_mismatch` |

The producer contract is: set `CommandRouting::Direct` (the default) when the
argv launches directly on the host; set `CommandRouting::Containerized` only
when the argv was genuinely wrapped in `<runtime> run …` and will be spawned
argv-direct through that runtime (the shell tool's container route). Never from
a bare `cwd`, and never for an invocation that was planned but not routed.

### Known limits of the landed slice (recorded honestly)

- **The marker is a producer contract assertion, not an unforgeable
  capability.** It is a field on facts that the producing tool fills in; a
  buggy or malicious producer that sets `Containerized` on an unwrapped argv
  would be admitted, and the only trace is the audited argv. There is no
  independent proof (e.g. re-inspecting the spawned process) that the command
  actually ran inside a container. Treat the marker as a *required assertion
  that catches the realistic mistake* — a caller that selected `Containerized`
  and forgot to route — not as a sandbox guarantee on its own. The kernel
  boundary is the runtime, not the marker.
- **Windows is unsupported and fails closed**, unchanged from §5: no runtime
  probe success ⇒ `sandbox_containerized_runtime_unavailable` (the tools seam
  can also surface `sandbox_containerized_unsupported_platform`). A first-class
  Windows story requires a superseding ADR.
- **Selection is programmatic only.** There is no config key; a caller must set
  `PolicyAction::sandbox_profile` and configure `ContainerConfig` on the shell
  tool. The path is unreachable from config/UI/CLI today, so the feature is
  inert in normal operation.
- **Mount, image, and pull behavior:** the project root is bind-mounted
  read-write at its own absolute path (`-v <root>:<root>`, the only default
  mount; further `-v` entries come only from the operator's `extra_mounts`), a
  `cwd` outside the root is refused, the image is operator-supplied, and **no
  image is pulled implicitly** — a missing image fails at the runtime,
  fail-closed. The plan also passes `--rm --init`. Network defaults to
  `--network none`; only explicitly listed `KEY=VALUE` entries are forwarded
  with `-e`, so provider keys (row 47) do not leak into model-driven processes.
- **`ReadOnlyFs` and `NetworkIsolated` remain stubs** (`sandbox_profiles_not_implemented`) and plugin execution under `Containerized` remains out of scope — the WASM capability sandbox still governs plugins.
