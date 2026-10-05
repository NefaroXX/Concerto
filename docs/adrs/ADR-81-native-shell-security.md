# ADR-81: Native shell execution and user-owned security settings

**Status:** Accepted for implementation (2026-10-02)

## Decision

Concerto's native command runtime is the default command environment. Native
process requests contain a program and argument vector and never implicitly
launch a system command interpreter. Interpreter compatibility is an explicit,
user-enabled mode. This supersedes ADR-28/30's requirement that every command
use the selected external-shell profile; profiles remain compatibility settings.

Shell security is user-global configuration, loaded independently of project
configuration and environment overrides. A project cannot grant itself more
authority. Desktop Settings and the human CLI edit this configuration; no agent
tool changes policy. Settings apply to the next command session/run, with that
limitation visible in the client. Policy evaluation and approval remain in
ToolExecutor. Native process tools use the canonical `shell` policy vocabulary
so switching executors does not discard existing approval rules.

The executor resolves native host executable paths and working directories
before policy evaluation and approval, and executes that prepared request.
This prevents a repeated PATH/symlink lookup from changing the selected binary
while approval is pending. Replacing the binary at its canonical path remains
an ambient OS risk in host mode.

Execution has two explicit boundaries: native host execution (ambient OS
permissions) and isolated execution through Docker/Podman. Offline execution
requires the isolated backend; it is never simulated by command-name filtering.
Unsupported confinement/resource settings refuse execution. Windows native
process execution is supported; container isolation retains ADR-72's Windows
refusal. No mode silently falls back to an external shell or weaker isolation.

The native runtime and clients share the same executor and structured result
contract. Built-in filesystem actions use the existing filesystem tool and VFS.
External process writes in host mode are not staged; the client must describe
that limit. Process cleanup, output ceilings and environment selection are
owned by the execution backend. A process-group kill is lifecycle management,
not proof of confinement against a hostile child that creates a new session.

## Delivery and verification

Implement a validated settings schema, protected config loading, direct process
executor, native runtime adapters, client settings and command surfaces, and
negative tests for configuration overrides, interpreter execution, invalid
limits, environment leakage, unsupported isolation and cancellation. Expand
platform confinement only with a working backend and native integration tests.

This decision does not claim equivalent kernel guarantees on every platform.

Automatic test validation in native runs uses an eval process-execution seam
backed by the same tool executor and session, including supervised execution.
It must not bypass shell security through a selected interpreter profile.
Coverage helpers without a governed implementation refuse execution in this
mode. Agent environment cards describe the argv-direct contract explicitly.
Capabilities and unsupported controls must remain inspectable. OpenShell is
not required by the native shell and can be added behind a future backend.
