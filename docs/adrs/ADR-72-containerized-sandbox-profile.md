# ADR-72: Containerized sandbox profile — OS-level isolation via a container runtime

**Status:** Accepted (2026-09-26) — in force for the `Containerized` profile.
`ReadOnlyFs` and `NetworkIsolated` remain explicit unimplemented stubs (§1).
Windows is not supported and fails closed (§5); that *decision* is settled, its
Windows *story* is still open.
**Date:** 2026-09-26
**Deciders:** Concerto architecture + maintainer direction
**Implemented by:** commits `d00582b` (core policy gate + runtime detection),
`3ae6ea5` (shell routing through docker/podman), `fdf4800` (`CommandRouting`
marker).

Composes with ADR-62 (ToolExecutor + `VirtualFs`), ADR-28 §6 (structured command
facts), and ADR-55 §2 (shell containment + intent gating). Supersedes: none.
Addresses security-threat-model §6 gap #1 ("No Containerized Plugin Sandbox")
and DEFERRED row 36.

## Context

`SandboxProfile` (`crates/core/src/types.rs`) declares four isolation levels and
enforces none of them. `SimplePolicyEngine::check_sandbox`
(`crates/core/src/policy.rs`) rejects every non-`None` profile with the single
rule name `sandbox_profiles_not_implemented`, so the enum was a stub: `None` ran
with the invoking user's ambient authority and no other profile could ever be
selected. Plugins were confined only by the WASM capability sandbox; the shell
ran with full user authority (modulo the ADR-55 containment heuristic, the
hardcoded shell denylist, and the row-45 CPU budget).

The trust boundary is model-authored code. A shell command or plugin chosen by
the model is untrusted input; the policy engine, approval sink, and containment
scan are in-process guards that share the process's authority. They reduce risk
but cannot contain a deliberately hostile command, because nothing sits between
the command and the host kernel. `Containerized` is the profile whose whole
purpose is to supply that missing kernel-level boundary, and it is the variant
this ADR makes real.

Row 42 (argv-direct execution + cmd quoting) and row 45 (process-group CPU
watchdog + `ulimit` backstop) already shape how a command is launched. This ADR
composes with them, not replaces them.

## Decision

### 1. Profile semantics

| Profile | Meaning | Enforcement |
| --- | --- | --- |
| `None` (and `None` field) | No sandboxing; run as the invoking user. | Unchanged — current behavior, byte-identical. |
| `ReadOnlyFs` | Filesystem writes denied, reads allowed. | **Still not implemented.** Denied by policy (`sandbox_profiles_not_implemented`). Implementing it is a separate ADR (Landlock/`ro` bind mounts). |
| `NetworkIsolated` | Network egress denied. | **Still not implemented.** Denied by policy (`sandbox_profiles_not_implemented`). Plugin egress allowlisting (row 46) is adjacent but not this profile. |
| `Containerized` | The command is executed inside an OS-level container with its own filesystem/network/process view. | Enforced: opt-in runtime detection + policy admission + container invocation routing (§2–§4). |

`SandboxProfile` is therefore partially real: two variants remain explicit stubs
and `Containerized` has a real detection, enforcement, and invocation path.

`None` semantics are frozen. Every existing call site passes
`sandbox_profile: None`, so the `Containerized` path is unreachable until a
caller selects it — which makes this slice behavior-preserving for every
current run (§6).

### 2. Trust boundary, routing contract, and fail-closed matrix

The container runtime is the boundary. Everything inside the container is
untrusted; the host mounts into the container are the deliberate, enumerable
attack surface.

`Containerized` admission needs three conditions beyond "runtime found" and
"plan is container-routable": the action's `CommandPolicyFacts` must also assert
`CommandRouting::Containerized` (`crates/core/src/types.rs`). A working
directory proves nothing about routing — a shell invocation can carry a `cwd`
and still be launched unconfined on the host — so the marker is what makes the
claim checkable at the policy seam. The engine enforces **both** directions, and
every denial is a named rule recorded in the audit trail.

| Condition | Verdict | Rule name |
| --- | --- | --- |
| `None` / `Some(None)`, no routing marker | pass to normal rule evaluation | — |
| `None` / `Some(None)`, routing marker **asserted** | `Deny` | `sandbox_container_routing_profile_mismatch` |
| `ReadOnlyFs`, `NetworkIsolated` | `Deny` | `sandbox_profiles_not_implemented` |
| `Containerized`, no runtime binary found | `Deny` | `sandbox_containerized_runtime_unavailable` |
| `Containerized`, runtime found, action has no container-routable plan | `Deny` | `sandbox_containerized_unenforceable` |
| `Containerized`, runtime found, well-formed plan, **no** routing marker | `Deny` | `sandbox_containerized_routing_missing` |
| `Containerized`, runtime found, well-formed plan, marker present | pass to normal rule evaluation | — |

**Producer contract for the marker:** set `CommandRouting::Direct` (the default)
when the argv launches directly on the host; set `CommandRouting::Containerized`
only when the argv was genuinely wrapped in `<runtime> run …` and will be
spawned argv-direct through that runtime (the shell tool's container route).
Never from a bare `cwd`, and never for an invocation that was planned but not
routed.

**Recorded limit, and it is a real one:** the marker is a *producer-contract
assertion, not an unforgeable capability*. It is a field on facts the producing
tool fills in; a buggy or malicious producer that sets `Containerized` on an
unwrapped argv would be admitted, and the only trace is the audited argv. There
is no independent proof (e.g. re-inspecting the spawned process) that the command
actually ran inside a container. Treat the marker as a *required assertion that
catches the realistic mistake* — a caller that selected `Containerized` and
forgot to route — not as a sandbox guarantee on its own. **The kernel boundary
is the runtime, not the marker.**

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
- Detection lives in `core::sandbox` (`crates/core/src/sandbox.rs`) as a
  `ContainerRuntime` enum, behind a `ContainerRuntimeProbe` trait so the policy
  engine and the shell tool can be tested with injected found/absent/malformed
  results and do not need a container in CI. `SimplePolicyEngine` takes the
  trait; its default is the system probe.
- The system probe caches its result behind a refreshable cell
  (`SystemContainerRuntime::new()`), so repeated policy evaluations do not
  rescan `PATH`; `refresh()` invalidates the cache for a runtime installed
  mid-session.

### 4. Container invocation (composes with rows 42/45)

`tools::container` builds the `docker`/`podman run` argv from an
already-planned `ShellPlan` and returns it as `ShellPlan::Direct`, so the shell
tool spawns it through exactly the path any other command takes
(`ShellTool::with_container`, plus an injectable
`with_container_runtime_probe` seam). When a container-routable plan is executed
under `Containerized`:

- The existing `ShellPlan` (row 42) is the **inner command**, wrapped as:
  `Direct { program, args }` → `[program, args…]`;
  `Wrapped { program, switches, operand }` → `[program, switches…, operand]`.
  The container runtime argv is spawned **argv-direct** (`ShellPlan::Direct`
  with `program = <runtime>`), so no host shell is introduced by the sandbox.
- The plan also passes `--rm --init`.
- The project root is bind-mounted read-write at its own absolute path
  (`-v <root>:<root>`) — the only default mount; any further `-v` entry comes
  only from the operator's `extra_mounts`. The container working directory is
  the same effective `cwd`; a `cwd` that does not fall under the mounted project
  root is refused (fail-closed) rather than mounted implicitly.
- Network defaults to **none** (`--network none`) and is opt-in per config.
- Host environment is **not** forwarded wholesale; only explicitly configured
  `KEY=VALUE` entries are passed with `-e`. This avoids leaking provider keys
  and credentials (row 47) into model-driven processes.
- The row-45 CPU budget is **not duplicated**: the `ulimit` prelude built by
  `with_cpu_limit` is already embedded in a `Wrapped` operand and therefore
  rides inside the container command unchanged; the process-group watchdog
  still supervises the `docker`/`podman` process from the host. The container
  runtime is not asked to re-impose a CPU ceiling.
- The image is operator-supplied and **no image is pulled implicitly**. A
  missing image fails at the runtime, fail-closed.
- `command_facts` is built from the same container plan, so the audited argv is
  the argv that runs.

### 5. Windows — decided, not supported

Windows has no `/bin/sh` and no `docker run`-compatible shell assumption, and
the repository hard-denies `unsafe_code`, which blocks the `windows-sys` FFI
(`CreateJobObjectW`/`AssignProcessToJobObject`) needed for a first-class Job
Objects implementation. Therefore:

- **Decision (v1, in force):** the `Containerized` profile is **not supported on
  Windows**. The runtime probe returns `Unavailable`, so the policy engine
  refuses `Containerized` with `sandbox_containerized_runtime_unavailable`; the
  tools seam can additionally surface `sandbox_containerized_unsupported_platform`.
  This is fail-closed and honest — an explicit refusal, never a silent
  unconfined run.
- **Still open (requires a superseding ADR; neither is in scope here):** a
  future Windows story could use Job Objects for process/memory limits, but
  kernel-level filesystem/network isolation on Windows would need either a
  container runtime (Docker Desktop / Windows Containers) with a
  Windows-compatible invocation shape, or a narrowly scoped, audited `unsafe`
  FFI dependency. Both are recorded as a residual, not as a decision.

### 6. Migration and rollback

- No data or config format changes. Selection of the profile is caller-driven
  (the `PolicyAction::sandbox_profile` field); no config key is added, so no
  migration is required.
- **Selection is programmatic only, and that is a recorded limit, not an
  oversight:** a caller must set `PolicyAction::sandbox_profile` *and* configure
  `ContainerConfig` on the shell tool. There is no config key, Settings surface,
  or CLI command, so the path is unreachable from config/UI/CLI and the feature
  is inert in normal operation.
- Rollback is trivial: `None` is the default and every existing call site uses
  it, so reverting the enforcement change restores the prior stub behavior
  exactly. Because the container route is opt-in, disabling it is a code/config
  change at the call site, not a data migration.

## Consequences

- `SandboxProfile` becomes partially real: two variants remain explicit stubs,
  `Containerized` gains a detection + enforcement + invocation path.
- `SimplePolicyEngine` gains an injectable runtime probe; its default is the
  system probe. Existing behavior for `None` and the two stub profiles is
  unchanged.
- The shell tool gains an opt-in container configuration; with it unset the
  spawn path is byte-identical to today.
- A container runtime present on a host no longer causes silent confinement —
  it only permits a caller that explicitly selects `Containerized` to proceed.
- The audit trail gains three named rules for this profile
  (`sandbox_containerized_runtime_unavailable`,
  `sandbox_containerized_unenforceable`, `sandbox_containerized_routing_missing`)
  plus `sandbox_container_routing_profile_mismatch` in the other direction, so
  a denied or mismatched container invocation is reviewable after the fact.

## Verification

- Deterministic unit tests cover detection (found / absent / malformed),
  policy enforcement (allow-with-runtime, refuse-without, unenforceable plan,
  missing routing marker, `None` unchanged, stub profiles still denied), named
  audit rows in both routing directions, and container invocation construction
  (argv, `--rm --init`, env, mounts, network, inner-command composition with the
  row-42 plan and row-45 `ulimit` prelude) **at the seam**.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo deny check`.
- No test or CI job requires a container runtime to be installed.

## Residuals

Recorded so the boundary of this decision stays explicit; each is stated in full
in the section it belongs to.

- Full plumbing of a user-facing config key that selects `Containerized` for a
  session is not part of this ADR; selection is programmatic for now (§6).
- The routing marker is a producer assertion, not proof of confinement (§2).
- `ReadOnlyFs` and `NetworkIsolated` remain stubs with their own ADRs (§1).
- Windows kernel-level isolation is out of scope; the refusal is in force and
  the Windows story is open (§5).
- Plugin execution under `Containerized` (rather than the WASM capability
  sandbox) is out of scope; this ADR governs the shell/process path.

---

*Last updated: 2026-09-26*
