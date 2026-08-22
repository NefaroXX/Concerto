# Policy rules

Policy rules decide whether a registered agent tool call is allowed
automatically, requires approval, or is denied. The Settings editor uses plain-
language actions and conditions; the configuration file uses the exact values
below.

## Evaluation order

Rules are evaluated from top to bottom and the **first matching rule wins**.
Put narrow exceptions before broad catch-alls. In the policy engine, an
unmatched call is denied.

The desktop runtime has an explicit expert/no-rules behavior: when no policy is
configured, it installs an allow-all policy rule so an empty list does not make
the Coder unusable. This does not bypass the shell tool's independent hard
denylist. If you want deny-by-default behavior, configure explicit rules and a
final Deny/Every operation rule.

## Actions

| Settings label | TOML `action` | Result |
|---|---|---|
| Allow automatically | `auto_approve` | Execute without prompting |
| Ask for approval | `require_approval` | Present the approval flow |
| Deny | `auto_deny` | Reject the call |

The schema also recognizes advanced internal actions
`require_managed_tool_approval`, `require_toolchain_approval`, and
`deny_network_egress`. They are not offered by the current Settings editor; use
them only when you understand the corresponding policy-engine behavior.
Unknown action strings are converted to deny.

## Conditions available in Settings

| Settings condition | TOML field(s) | Accepted value |
|---|---|---|
| Tool is | `tool_name` | `filesystem` or `shell` |
| Tool operation is | `tool_name`, `operation` | tool `filesystem`; Settings currently offers `read`, `write`, `delete`, or `exists` |
| Project path matches | `path_glob` | project-relative glob, for example `src/**`, `**/*.rs`, or `**/*` |
| Shell command matches | `command_pattern` | Rust regular expression, for example `^cargo (check|test)$` |
| Every operation | `always = true` | catch-all; put it last |

Path conditions apply to filesystem input paths. Command patterns apply to the
shell command text. Invalid regular expressions are rejected by validation or
never match; do not treat a malformed deny expression as protection.

The filesystem tool also implements `list`, but the current Settings operation
picker does not expose it. A Tool is `filesystem` rule can cover list calls; a
manually authored `operation = "list"` condition is understood by the policy
engine. This UI omission is a known configuration gap, not a different tool
name.

The schema also supports `git_operation`, but the current desktop runtime
registers only `filesystem` and `shell` tools. Do not create a `git` rule in the
UI expecting it to match shell commands such as `git status`; use a shell
command regular expression instead.

The schema also supports `tool_name_prefix` — a prefix/glob tool-name condition
(e.g. `mcp:github:`) used for MCP server-level rules. The current Settings
editor does not offer it; author these rules in the config file (see the MCP
section below).

## Examples

Read files automatically, ask before writes/deletes, allow common Cargo checks,
then ask about everything else:

```toml
[policy]

[[policy.rules]]
action = "auto_approve"
[policy.rules.condition]
tool_name = "filesystem"
operation = "read"

[[policy.rules]]
action = "require_approval"
[policy.rules.condition]
tool_name = "filesystem"
operation = "write"

[[policy.rules]]
action = "require_approval"
[policy.rules.condition]
tool_name = "filesystem"
operation = "delete"

[[policy.rules]]
action = "auto_approve"
[policy.rules.condition]
command_pattern = "^cargo (check|test|fmt|clippy)( |$)"

[[policy.rules]]
action = "require_approval"
[policy.rules.condition]
always = true
```

Protect a path before a broader write rule:

```toml
[[policy.rules]]
action = "auto_deny"
[policy.rules.condition]
path_glob = "secrets/**"

[[policy.rules]]
action = "require_approval"
[policy.rules.condition]
tool_name = "filesystem"
operation = "write"
```

Because first match wins, reversing those two rules could allow the broad write
rule to shadow the protected path.

## MCP tools and policy

MCP tools arrive in the registry namespaced `mcp:<server_id>:<tool_name>` and
are ordinary tools under `ToolExecutor` — policy, spend, audit, and events
apply unchanged (ADR-43 §6). MCP servers are network-capable child processes,
so they are never implicitly auto-approved:

- **Default posture.** When `[mcp]` is enabled and at least one server is
  enabled, the orchestrator appends a `require_approval` rule for
  `tool_name_prefix = "mcp:"` *after* your rules
  (`crates/orchestrator/src/runtime_runner.rs`). Unmatched MCP tools therefore
  ask for approval; because rules are first-match-wins, an explicit user rule
  placed earlier overrides the preset.
- **Server-level rules** use `tool_name_prefix`, which matches any tool whose
  name starts with the prefix (for example `mcp:github:`).
- **Allow one trusted server, deny the rest** (allow rule must come first):

```toml
[[policy.rules]]
action = "auto_approve"
[policy.rules.condition]
tool_name_prefix = "mcp:github:"

[[policy.rules]]
action = "auto_deny"
[policy.rules.condition]
tool_name_prefix = "mcp:"
```

- **Ask before every MCP call** (equivalent to the runtime default preset; only
  needed if you want it explicitly, e.g. with `mcp.enabled` on and rules loaded
  before the preset):

```toml
[[policy.rules]]
action = "require_approval"
[policy.rules.condition]
tool_name_prefix = "mcp:"
```

Policy gating is by tool name plus explicit enablement; `DenyNetworkEgress`
cannot see inside a server's own traffic. MCP servers are trusted child
processes — the boundary is OS process isolation plus policy
([mcp.md](mcp.md)).

## Shell safety is separate

Policy approval is necessary but not always sufficient for a shell command.
`ShellTool` validates the executable/arguments, applies timeouts and output
caps, keeps execution rooted in the project, and consults a hard denylist before
any allow-all mode. A command denied by that layer remains denied even if a
policy rule says Allow automatically.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| “Access is denied” from a filesystem call | OS permissions, missing/incorrect project root, selected shell/runtime path, or policy rejection; capture the tool input and error source |
| A rule never matches | Wrong tool, operation spelling, path not project-relative, invalid regex, or an earlier rule matched first |
| All tools are denied | Configured rules have no matching allow/ask catch-all; add an appropriate final rule |
| No rules but tools run | Expected desktop expert/no-rules behavior; shell hard-denies still apply |
| Git command does not match `git_operation` | Git is being invoked through `shell`; use `command_pattern` |

Policy settings are powerful. Test new rules on a disposable project and review
the Tool Log before using them on important work.
