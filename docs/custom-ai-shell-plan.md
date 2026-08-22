# Concerto AI-native shell implementation plan

**Status:** Phases A and B implemented as a library foundation; Phases C–F planned

**Started:** 2026-07-18

**Current home:** `crates/shell`; not yet exposed as the desktop terminal runtime

**Research (2026-08-01):** [research brief](research/ai-native-shell-research-brief.md)
(failure modes + prior art), [implementation plan](research/ai-native-shell-implementation-plan.md)
(fresh phase plan starting with a `ToolManifest` schema system — its phase
numbering must be reconciled with section 5 below before starting),
[expanded reference](research/ai-native-shell-expanded-research.md)
(concrete schemas, validation layer, provider-native schema conversion,
hallucination benchmark spec).

## 1. Goal

Build a project-scoped command environment that is equally usable by people and
Concerto agents. It should make automation more reliable than driving an
unstructured terminal while continuing to interoperate with Bash, PowerShell,
and other installed shells.

The target is not a replacement for every feature of Bash. It is a typed command
runtime with:

- stable, machine-readable results and useful human rendering;
- explicit project, session, provider, model, agent, and memory context;
- declared effects that the existing policy engine can evaluate;
- recoverable failures that do not terminate a shell or agent session;
- AI-native commands such as `explain`, `debug`, and `optimize`;
- deterministic workflows that agents can generate and revise safely;
- adapters for existing executables, shell profiles, and future Concerto tools.

## 2. Boundaries

Three related components remain distinct:

| Component | Responsibility | Lifecycle |
| --- | --- | --- |
| Desktop terminal | Interactive PTY and terminal emulation | Long-lived |
| Agent shell tool | Policy-gated, one-shot process execution | Per tool call |
| Concerto shell runtime | Typed commands, context, workflows, and rendering | Long-lived session |

The Concerto shell may launch an external shell through a policy-gated adapter,
but it must not absorb the PTY implementation or bypass `ToolExecutor`.

## 3. Core contracts

### 3.1 Command result envelope

Every command returns a versioned `CommandResult`, including commands that fail.
The envelope contains:

- command name and status;
- optional process exit code;
- structured JSON data;
- human-readable summary;
- diagnostics with stable codes and severity;
- artifacts and suggested next actions;
- elapsed time and provenance.

The initial status model is:

| Status | Meaning | Continue session? |
| --- | --- | --- |
| `succeeded` | Requested operation completed | Yes |
| `succeeded_with_warnings` | Completed with non-fatal diagnostics | Yes |
| `recoverable_failure` | Bad input or a correctable operational failure | Yes |
| `awaiting_approval` | Policy requires a user decision | Yes |
| `blocked` | Policy deliberately denied the effect | Yes |
| `cancelled` | Caller cancelled the operation | Yes |
| `terminal_failure` | Runtime invariant or infrastructure is unavailable | No |

This contract prevents an invalid argument, missing file, non-zero exit, bad tool
call, or exhausted retry from becoming an application-level crash. Only a
genuinely terminal status should stop an unattended workflow.

### 3.2 Explicit context snapshot

Commands receive an immutable `ShellContext`, not ambient UI state. The snapshot
will grow in a backwards-compatible way and initially includes:

- canonical project root and current directory;
- optional project and session identifiers;
- active provider and model;
- selected agent roles;
- optional source-control branch.

Secrets and the process environment are deliberately excluded.

### 3.3 Declared effects

Each command declares its possible effects: project read/write, process spawn,
network access, Git mutation, agent invocation, and memory read/write. These are
effect claims, not capability tiers. Policy decisions remain explicit and
auditable per operation.

The runtime must refuse to add process, write, network, Git, memory-write, or
agent commands until an effect-policy adapter is present. Read-only built-ins are
safe for the first vertical slice.

### 3.4 Command registry

Commands expose a stable specification: name, description, usage, source,
effects, and whether their results enter history. Built-ins, external adapters,
AI commands, workflows, and plugins use the same registry contract.

## 4. Language approach

Do not begin with a general-purpose parser. The progression is:

1. command plus quoted arguments;
2. typed command options and structured values;
3. a serializable workflow AST with variables, conditions, bounded loops,
   parallel groups, retries, and approval nodes;
4. a compact Rust-like surface syntax that compiles to the AST.

The AST is the stable automation format. Surface syntax can evolve without
invalidating saved workflows. Arbitrary `eval`, implicit shell expansion, and
unbounded agent-generated loops are out of scope.

## 5. Delivery phases

### Phase A — foundation and read-only vertical slice

**Implemented.** The crate includes typed results/diagnostics, context/effects,
registry/history/runtime, deterministic parsing, and `help`, `project-info`,
`ls-tree`, and `last`.

- Add `concerto-shell` as an independent library crate.
- Implement the result, diagnostic, context, effect, command, registry, history,
  and runtime contracts.
- Parse quoted command lines without shell expansion.
- Ship `help`, `project-info`, `ls-tree`, and `last`.
- Support pretty JSON serialization for every result.
- Test continuation after parse errors, unknown commands, missing paths, and
  cancellation.

Exit criterion: a caller can run multiple commands in one runtime and every
ordinary failure remains inspectable and recoverable.

### Phase B — policy-gated external execution

**Implemented at the library boundary.** `run`, `shell-run`, and
`shell-profiles` route through a `PolicyExecutionAdapter` and the canonical
profile catalog. Frontend exposure still requires live-tested integration.

- Consume the one canonical shell profile selected for agent execution (ADR-30).
- Add a direct executable adapter and an explicit shell-script adapter.
- Route both through the existing policy engine and `ToolExecutor`.
- Preserve stdout, stderr, exit status, timeout, cancellation, and retry advice
  in `CommandResult`.
- Never infer approval from an effect declaration; evaluate the concrete call.

Profile discovery, persistence, managed-runtime state, and canonical selection are
owned by `concerto-config`. The AI shell receives the resolved catalog instead
of maintaining another selector. Concerto should not bundle an additional shell
in this phase.

#### Phase B contract decisions

- `run` accepts an executable and argument vector; `shell-run` accepts a script
  plus a validated interpreter profile. Neither command spawns a process itself.
- Both commands call the existing `ToolExecutor` with the concrete executable,
  arguments, working directory, and timeout. Policy, approval, audit, the tool's
  hard denylist, project-root enforcement, cancellation, and spawning remain in
  that single path.
- Command-pattern policy evaluates the actual executable plus arguments. There
  is deliberately no caller-supplied policy description that could differ from
  the process being approved.
- A runtime with external execution refuses construction when its
  `ShellContext.project_root` differs from the executor session's project root.
- Interpreter invocation uses `ShellProfileConfig::command_args`, keeping Bash,
  POSIX shell, Command Prompt, PowerShell, Git Bash, and MSYS2 behavior aligned
  with the integrated terminal and agent shell.
- Process errors, non-zero exits, timeouts, denial, approval requirements, and
  cancellation are structured `CommandResult` values. Only runtime invariant or
  infrastructure failures may produce `terminal_failure`.

Phase B shares the existing Settings and desktop terminal profile contracts;
the typed runtime adds no second persistence or process-spawning boundary.

### Phase C — AI-native commands

- `explain <command-or-result>` produces an explanation without executing it.
- `debug <process-or-last-result>` gathers evidence and proposes bounded steps.
- `optimize <script-or-workflow>` generates candidates and validates changes.
- Agent calls use the explicit context snapshot and return the same result
  envelope.
- Automatic repair obeys configurable cycle and spend limits; exhaustion is a
  recoverable result with preserved evidence.

### Phase D — deterministic workflows

- Define a serde-versioned workflow AST.
- Add validation for cycles, missing variables, incompatible result types, and
  undeclared effects.
- Add checkpoints and resumable execution.
- Add bounded retry, fallback, approval, and parallel nodes.
- Render a complete execution trace suitable for both UI inspection and agent
  feedback.

### Phase E — tools, fixtures, and extensions

- Add a stable tool/plugin ABI around command specifications and results.
- Allow custom tools to add schemas, renderers, and policy effect declarations.
- Add signed or explicitly trusted installation flows.
- Use Concerto-generated projects as fixtures, starting with `oxide-serve`.

Each fixture receives a manifest describing setup, supported platforms,
commands, expected artifacts, assertions, cleanup, and failure classification.

### Phase F — measured self-improvement

- Mine command history for repeated failures and inefficient sequences.
- Suggest aliases, workflow rewrites, or targeted agent assistance.
- Require validation against fixture manifests before promotion.
- Keep autonomous application opt-in, scoped, reversible, and policy-gated.
- Record provenance and comparison evidence for every promoted change.

Self-improvement is an optimization loop over measurable outcomes, not permission
for the shell to silently rewrite itself.

## 6. Testing strategy

### Contract tests

- JSON round trips remain backwards compatible within the schema version.
- Every status has explicit continuation semantics.
- Registry duplication and invalid names are deterministic.
- Quoting and escaping never perform shell expansion.

### Safety tests

- `ls-tree` cannot escape the project root through `..` or a symlink.
- cancellation returns `cancelled` and leaves the runtime usable;
- unknown commands and malformed options return `recoverable_failure`;
- effectful commands cannot be registered in the standard runtime before the
  policy adapter exists.

### Fixture tests

Use small generated programs as black-box fixtures. `oxide-serve` should test:

- project discovery and structured tree output;
- build/run command result capture after Phase B;
- port-conflict and missing-file diagnostics;
- AI explanation and bounded repair after Phase C;
- repeatability of a saved workflow after Phase D.

## 7. Near-term implementation order

1. Live-test Phase A/B result and policy contracts independently.
2. Add an explicit experimental CLI entrypoint without replacing the existing
   terminal UI or agent shell tool.
3. Expose the runtime as an optional desktop terminal mode only after the CLI
   contract is stable.
4. Begin AI-native commands only after result envelopes and policy traces have
   survived live testing.

## 8. Deferred decisions

- Surface-language grammar and file extension.
- Whether workflows use a dedicated bytecode/interpreter layer.
- Plugin distribution and trust model.
- Rich terminal protocol beyond JSON and plain text.
- Whether the shell becomes a standalone binary or remains a Concerto mode.
- Default limits for autonomous optimization loops.

These decisions should be made from Phase A/B usage evidence rather than fixed in
the foundation.
