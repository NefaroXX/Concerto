# Shell profiles

Concerto uses one authoritative **Agent execution shell** selection. Agent shell
commands are the primary consumer; Validator commands and the integrated
desktop terminal follow the same profile so quoting, environment, and working
directory do not diverge.

## Profile list

Settings displays:

- shells detected on the current host; and
- profiles the user explicitly added.

It does not populate a permanent catalogue of shells that are not installed.
Stale auto-detected profiles are removed when settings are normalized on a new
host. Concerto Managed Bash appears only when its managed runtime is installed.

Typical Windows discoveries include Command Prompt, PowerShell, WSL, and Git
Bash when present. Typical Unix discoveries come from the login environment,
`/etc/shells`, and executable lookup. Detection does not prove that every tool
inside the shell is installed.

## Adding a custom profile

Use **Add profile**, then configure:

- a unique ID and friendly name;
- backend type;
- executable path;
- startup arguments and environment variables;
- working-directory behavior;
- optional toolchain/manifest metadata where supported.

Verify the executable before selecting it. A custom profile remains in the
list because it is user intent; if its executable later disappears, Concerto
should report it unavailable rather than silently changing the selection.

## Working directory

| Setting | Behavior |
|---|---|
| Project root | Start commands in the selected project |
| Home directory | Start in the user's home directory |
| Shell default | Let the shell choose its startup directory |

Agent coding workflows normally need **Project root**. Choosing another value
can make relative file and validation commands operate outside the expected
directory.

## What the selection controls

| Surface | Uses selected profile? |
|---|---|
| Agent `shell` tool | Yes; this is the authoritative use |
| Multi-agent Validator commands | Yes |
| Integrated desktop terminal | Yes |
| External terminal applications | No |
| `concerto-shell` typed runtime | It can consume profiles, but it is not yet the desktop terminal runtime |

Changing the selection affects newly launched commands/terminals. Restart an
already-running terminal session if it was created under the old profile.

## Troubleshooting

| Error | Checks |
|---|---|
| “The system cannot find the file specified” | Executable path exists, WSL/distribution is installed, custom startup argument is valid |
| “Access is denied” | Executable permission, project-directory ACL, antivirus/application control, and whether a directory was supplied as an executable |
| Agent and terminal behave differently | Confirm both were started after the same selection was saved; capture resolved executable/args/CWD |
| Shell appears twice | Remove the explicit custom duplicate; detected profiles are regenerated |
| Shell is not detected | Add it as a custom profile with an absolute executable path |

See [ADR-28](adrs/ADR-28.md) for the profile schema
and [ADR-30](adrs/ADR-30.md) for the unified
selection decision.
