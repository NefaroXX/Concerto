# ADR-29: AI-native shell runtime and policy-gated execution

**Status:** Accepted
**Date:** 2026-07-18
**Deciders:** Concerto architecture

## Context

Concerto needs a command environment that agents can operate reliably without
depending on terminal text scraping. It must return structured results, retain
project and agent context, continue after ordinary command failures, and support
the interpreter profiles established by ADR-28.

This is distinct from the desktop PTY. The PTY is an interactive user terminal,
while agent and workflow execution is non-interactive and already subject to
`ToolExecutor`, policy evaluation, approval, audit, tool deny rules, project
sandboxing, timeout, and cancellation. A second process-spawning path in the
new runtime would create a policy bypass and divergent error semantics.

## Decision

Add `concerto-shell` as a typed, long-lived command runtime with a versioned
`CommandResult`, immutable `ShellContext`, declared command effects, a registry,
history, and recoverable status semantics.

- Only `terminal_failure` stops the runtime. Parse errors, invalid arguments,
  non-zero exits, timeouts, policy denial, approval requirements, cancellation,
  and exhausted bounded retries remain structured, continuable results.
- The standard runtime accepts only commands whose effects are project reads.
- Effectful external commands are installed only by a constructor that receives
  a `PolicyExecutionAdapter` backed by the existing `ToolExecutor`.
- The runtime verifies that its project root and the executor session's sandbox
  root identify the same project before registering external commands.
- ADR-28's `ShellSettings` and `ShellProfileConfig` are the only persisted
  profile definitions. The AI shell consumes the binding selected in Settings
  and never maintains a second profile configuration or discovery path.
- `run` passes an executable and argument vector directly to the existing shell
  tool. `shell-run` resolves a configured interpreter and passes its executable
  plus arguments through the same policy-gated path.
- Command-pattern policy and approval summaries are reconstructed from the
  actual executable and actual arguments. Callers cannot supply an alternate
  policy description that differs from the process being approved.

## Consequences

- Human and agent frontends can consume the same typed result and continuation
  contract without treating an ordinary tool error as a crashed session.
- External commands retain Concerto's existing policy and audit boundary; the
  shell runtime has no direct process API.
- Direct executable spawning is required behind the tool gate for typed
  adapters, avoiding accidental double shell wrapping of interpreter profiles.
- Settings, the desktop terminal, agent execution, and the AI shell share the
  same stable profile IDs and bindings.
- Command-pattern rules describe the complete executable and argument text,
  which also makes deny patterns inspect arguments rather than only a binary
  name.
- The initial command parser is intentionally not a general-purpose language.
  Deterministic workflows will use a versioned AST before gaining a compact
  Rust-like surface syntax.

## Alternatives Considered

- **Reuse the desktop PTY for agents:** rejected because terminal emulation and
  text scraping do not provide typed results, deterministic boundaries, or
  one-shot policy evaluation.
- **Spawn directly from `concerto-shell`:** rejected because it duplicates and
  can bypass policy, approval, auditing, deny rules, and project sandboxing.
- **Maintain AI-shell-specific profiles:** rejected because two persisted
  selectors could disagree about the executable and policy identity in use.
- **Let callers provide a separate policy command string:** rejected because an
  untrusted caller could make the policy evaluate text other than the process
  that will actually run.
