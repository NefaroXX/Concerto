# CLI settings and Studio commands

Run `concerto config help` for syntax. Standard builds automatically select
CLI mode for commands; `concerto --cli` opens the terminal interface.

## Shared configuration

```sh
concerto config keys retry
concerto config show --global
concerto config show --json
concerto config show --example
concerto config get retry.max_attempts
concerto config path --project-scope
concerto config set retry.max_attempts 4 --dry-run
concerto config set retry.max_attempts 4
concerto --project /path/to/project config set memory.enabled false --project-scope
concerto config unset memory.enabled --project-scope
concerto config set model_settings.global_default_model '"model-name"'
concerto config set project_context.enabled true
concerto config set skills.auto_load false
concerto config set mcp.enabled true
concerto config set plugins.enabled true
concerto config set session_spend_cap_usd 10.0
concerto config set policy.rules --file rules-value.toml
concerto config set shell_settings.profiles --file profiles-value.toml
concerto config set model_settings.providers.0.timeout_seconds 120
concerto config set multi_agent.coordinator_prompt --file prompt-value.toml
```

Values are TOML: booleans/numbers are bare, strings are quoted, arrays and
inline tables use TOML syntax. Shell quoting protects that TOML from the
invoking shell. PowerShell/cmd quoting differs; `--file` avoids nested quoting
and reads **one TOML value**, without a key or table header. For example,
`prompt-value.toml` can contain a triple-quoted multiline string.

`config keys` reflects serialized names; use these rather than guessing from
desktop labels. Numeric indexes address existing records. Replace whole
arrays to add/remove/reorder records, or use the lifecycle commands below.
`unset` removes saved values to restore defaults/inheritance. Reads default
to effective values; writes default to global; `--project-scope` writes only
the selected project's `.concerto.toml`. Inspection/dry runs redact env values.

## Agents and advisory blueprints

```sh
concerto agents list
concerto agents show coder
concerto agents set coder capabilities.shell false
concerto agents set coder disabled true
concerto agents set coder can_cover '["implement", "review"]'
concerto agents set coder prompt_sections.system_instructions '"Prefer small patches."'
concerto agents clone coder second-coder
concerto agents import agent.toml
concerto agents remove second-coder --yes
concerto blueprint list
concerto blueprint show > blueprint.toml
concerto blueprint select tdd
concerto blueprint import blueprint.toml
```

Import accepts the same flat per-agent TOML file as Desktop, including its
per-file `schema_version`. Edit long prompts in that file and import it back.
Cloning preserves settings/model selection and gives a new identity without
adding topology references. Removal cleans global assignment/relationship/
blueprint references; invalid remaining blueprints are rejected. The
Coordinator is runtime-owned; its supplementary prompt is
`multi_agent.coordinator_prompt`. Blueprints remain advisory.

## Providers and keys

```sh
concerto providers list
concerto providers add work openai https://api.openai.com/v1
concerto credentials set providers/work/api_key --prompt
concerto credentials status providers/work/api_key
concerto providers refresh work
concerto config set model_settings.providers.0.model '"model-name"'
concerto providers remove work --yes
concerto credentials delete providers/work/api_key --yes
```

Provider types: openai, anthropic, google, openrouter, nim, ollama, opencode.
For an existing route, read its `keyring_key` rather than assuming an account.
`--stdin` supports secret-manager piping. Keys cannot be supplied as arguments.
Remove agent/fallback assignments before removing a provider; deleting its
configuration does not delete its keychain credential.

## Structured TUI settings

F1/Ctrl+P → **Manage providers, agents and policy** (or command-mode `o`)
opens the structured settings browser, including MCP and all config sections.
Enter opens an object or array; `e` edits the selected value. Strings use a
plain multiline editor; other values use JSON. Arrow keys, Home/End,
Backspace/Delete, Enter and bracketed paste edit the draft. Ctrl+S validates
and saves; Esc asks before discarding it. Validation errors keep the draft.

`g` switches global/project scope; project mode shows effective values and
saves project overrides. Agent fields remain global and save to canonical
agent files. Roster membership changes use `agents import/clone/remove`.
Environment values remain hidden and are changed through CLI commands.
Refresh with `r` after an external edit: stale drafts are rejected instead of
overwriting newer settings. Config changes apply to the next run, and project,
environment or startup overrides may take precedence over a global save.
Use JSON `null` to remove a config override and restore inheritance.

## Memory commands

```sh
concerto memory list --json
concerto memory search "authentication" --type function --limit 50 --json
concerto memory show CHUNK_ID --json
concerto memory delete CHUNK_ID --yes
concerto memory reindex
concerto memory graph
concerto memory explain CHUNK_ID
```

The TUI command menu (F1/Ctrl+P) includes **Memory explorer**; command mode
also opens it with `e`. `/` edits a search, Enter applies it, Tab/Shift+Tab
cycles entry types, and Enter on a row opens its full content and metadata.
Use paging or Home/End in details. `d` asks for confirmation bound to the
selected chunk ID; only `y` deletes, while `n`/Esc cancels. `r` refreshes and
`R` re-indexes with the configured project exclusions. Esc returns to the list
and then chat, preserving the chat draft.

Search uses the same memory retrieval service as Desktop. Inspection and
deletion are explicitly project-scoped, including when a global memory entry
has the same ID. Re-indexing can restore file-backed chunks previously deleted
from retrieval. Memory must be enabled; standalone commands initialize the
normal memory subsystem, may start indexing, and obey its instance lock. The
TUI reuses a running project's initialized store. Closing cancels pending
work and discards late results. JSON returns full content with escaped control
characters; `--limit` is bounded to 1–1000 and defaults to 200.

## Extension and shell commands

```sh
concerto extensions skills list
concerto extensions skills show pack-id
concerto extensions skills create skill.toml /path/to/skill-packs
concerto extensions skills edit pack-id updated-skill.toml
concerto extensions skills disable pack-id
concerto extensions skills enable pack-id
concerto extensions skills remove pack-id --yes
concerto extensions mcp add server.toml
concerto extensions mcp set server-id args '["argument with spaces"]'
concerto extensions mcp set server-id timeout_secs 60
concerto extensions mcp probe server-id
concerto extensions mcp disable server-id
concerto extensions mcp remove server-id --yes
concerto plugin installed
concerto plugin install plugin.wasm
concerto plugin install plugin.wasm --replace
concerto plugin list
concerto plugin revoke plugin-id
concerto plugin remove plugin-id --yes
concerto shell list
concerto shell test PROFILE_ID
concerto shell select PROFILE_ID
concerto shell managed install /path/to/bash
concerto shell managed verify
concerto shell managed export runtime-manifest.json
concerto shell managed import runtime-manifest.json
concerto shell managed remove --yes
```

MCP input files are flat records with `id`, `command`, optional `args`,
`enabled`, `env`, `timeout_secs`. Credential-like `env` keys must be
`keyring:<account>` references — plaintext credential values are rejected when
the record is saved (see [mcp.md](mcp.md)). Skill create/edit accepts the
shared full SkillManifest: id/name/version/description, optional instructions,
instructions_path, tools/resources. SKILL.md-only edits remain file-based;
removing a skill keeps recoverable manifest backups.

Install/enable operations do not turn on master subsystem switches. Plugin
install does not grant capabilities. MCP probes explicitly start the configured
program, list tools, then stop it without changing saved switches.
Choose a shell profile ID from `shell list`; detected IDs depend on the host.
CLI runs build a fresh plugin manager. Restart Desktop after changing installed
plugin files through these commands; its runtime manager is retained across runs.

## Runtime inspection

```sh
concerto sessions list
concerto sessions runtime SESSION_ID
concerto sessions runtime SESSION_ID --json
concerto sessions runtime SESSION_ID --view decisions --limit 20
concerto sessions runtime SESSION_ID --view world --json
concerto sessions runtime SESSION_ID --view diagnoses
concerto sessions runtime SESSION_ID --view suitability
concerto sessions runtime SESSION_ID --view topology --json
concerto sessions runtime SESSION_ID --view evidence
concerto sessions runtime SESSION_ID --event EVENT_ID --json
```

Open the TUI inspector with **F1, then i** (or **Esc, then i**) for the active
session. Switch views with Tab/Shift+Tab, Left/Right, or keys 1–7: overview,
topology, decisions, evidence links, world model, diagnoses, suitability.
Switching views reuses the loaded checkpoint; `r` reloads it and keeps the
selected view. Use Up/Down or `j/k` to scroll, Page Up/Down to move one visible
page, Home/End to reach the start/end, and Esc to return to chat. Scrolling is
clamped to the report, including after terminal resize.

In the evidence view, `[` and `]` select a referenced event and Enter opens its
full stored record. The picker deduplicates and sorts decision citations,
fact/grounding/contradiction IDs, and artifact/question references. The selected
ID stays visible above the detail. Esc returns to the reference view; `r` reloads
an open event. Closing, changing selection/view/session, or refreshing cancels
the previous lookup so late results cannot replace the new selection.

Both frontends use the shared checkpoint projection and suitability ranking.
This shows **persisted checkpoint data**, not live Coordinator state; completed
or cleared checkpoints may have no inspection data. It does not pause, resume,
dispatch, or modify a run. Command mode can inspect alongside a running frontend:
its database connection is read-only and performs no initialization, migrations,
retention, recovery, or encryption setup. Pending encryption setup must be
completed by normal application startup; existing keys are required.

`--view` accepts `all` (default), `topology`, `decisions`, `evidence`, `world`,
`diagnoses`, or `suitability`. `--limit` is 1–1000 (default 50), applied to each
history and evidence-bearing decision list newest first, and to agents per
suitability class best first. The topology and world-model collections remain
complete so limits cannot sever graph edges or hide fact provenance.

Topology includes task IDs, parents, agent roles, descriptions, saved statuses,
declared dependencies, deliverables, and directed edges from prerequisite or
context provider to consumer. A saved `Running` status does not prove the task
is currently running. Evidence links include fact summaries with their original
status, grounding and contradiction references, artifact/question references,
and decision citations. These are checkpoint references; full whiteboard event
bodies are not loaded or independently verified by this view.

Use `--event EVENT_ID` (without `--view` or `--limit`) to resolve a full record
on demand. It reads the durable whiteboard event log through the same read-only
connection, scoped to `SESSION_ID`. JSON returns `source: "whiteboard_event"`,
`found`, and `event`, including payload, kind, agent, sequence numbers, causation,
hashes and timestamp. Missing IDs, references that identify decisions instead
of log events, and IDs belonging to another session return `found: false` with
`event: null` (successful lookup, no matching record). Storage failures and
unknown sessions still exit nonzero. Details can be read without a checkpoint;
a stored event can be newer than the checkpoint and is not a fresh verification.

JSON schema version 1 gains additive `topology` and `evidence` data sections and
task/edge/evidence-decision totals. Existing sections remain unchanged. JSON
includes `schema_version`, `session_id`, `source`, `empty`,
`limit`, unfiltered `totals`, and the requested `data` sections. Unknown sessions,
unavailable storage, and invalid checkpoints exit nonzero; an existing session
without a checkpoint returns an empty report. Inspection needs no provider or
setup wizard. Evidence references remain intact in the output.

## Appearance

### Terminal navigation and editing

The TUI separates the conversation, message composer, keyboard hints and run
status into their own rows. The composer grows for multiline drafts and keeps
the cursor visible when text is longer than the terminal width.

| Action | Keys |
|---|---|
| Open the command menu from chat | `F1` or `Ctrl+P`; `?` in browse mode |
| Choose a menu item | Up/Down, then Enter; Esc closes without changing the draft |
| Switch between typing and browsing | Esc |
| Send the current message | Enter, when no run is active |
| Insert a newline | `Ctrl+J` (Alt/Shift+Enter also works when reported by the terminal) |
| Edit within a draft | Left/Right, Home/End, Backspace/Delete |
| Read earlier/later conversation | PageUp/PageDown; Up/Down or j/k in browse mode |
| Jump to the oldest/latest output | Home/End in browse mode |
| Cancel a run / quit when idle | `Ctrl+C` |

Bracketed paste inserts text into the draft without sending it. Approval,
intent and plan prompts retain priority over pasting and navigation shortcuts.
While a run is active, the composer keeps the next draft until it can be sent.
Opening settings or cancelling project selection preserves the draft.

Chat follows the latest output until you scroll back; new output leaves your
reading position in place. Returning to the bottom resumes following. Sessions,
agent models, tool history and the command menu scroll their selected row into
view. Tool history supports Up/Down, Home/End and `c` to clear the displayed log.

### Theme and motion preferences

```sh
concerto config set display.theme '"Slate"'
concerto config set display.reduced_motion true
concerto config set display.animated_terminal_title false
concerto config set display.muted_agents '["reviewer"]'
concerto preferences show
concerto preferences set ui_theme Slate
concerto preferences set ui_font_size 16
```

`display.theme` controls CLI colors. `preferences ui_theme/ui_font_size`
controls Desktop's existing startup store (font size 12–20). Close a running
Concerto instance before editing a locked preferences store.

Changes follow existing next-run/startup semantics. Project/env/startup flags
may override saved values. See [the capability plan](desktop-cli-parity.md)
for remaining interaction work and the runtime boundaries.


## Workspace files and context

These commands require no provider. Put startup options such as `--project DIR`
before `workspace`; its operands, including `--`, belong to the command.

```text
concerto --project DIR workspace list [SUBDIRECTORY] --json
concerto workspace read src/main.rs --json
concerto workspace read src/main.rs --raw > edited.rs
concerto workspace search "TODO" src/main.rs src/lib.rs --json
concerto workspace context src/main.rs src/lib.rs --json
concerto workspace write new.rs --stdin --create --yes < edited.rs
concerto workspace write src/main.rs --stdin --expect REVISION --yes < edited.rs
concerto workspace delete obsolete.rs --expect REVISION --yes
concerto workspace diagnostics src/main.rs --json
concerto workspace hover src/main.rs 10 4 --json
concerto workspace definition src/main.rs 10 4 --json
concerto workspace completion src/main.rs 10 4 --json
concerto workspace references src/main.rs 10 4 --json
```

For external editing, retain the `revision` returned by `read --json`, export
with `read --raw`, then open the exported buffer in your preferred editor. Its
find/replace, undo/redo, indentation and navigation work normally. Review the
edited buffer and import it with `write --stdin --expect REVISION --yes`. A
changed original is rejected; reload and reconcile before retrying. `--raw`
requires redirected stdout; JSON safely escapes terminal controls. Keep the
buffer until the write succeeds. Concerto does not launch an ambient `$EDITOR`
command, and no shell-string parsing is involved in the import.

All workspace file operations go through the shared policy/executor and audit
path. Explicit mutation commands supply operator approval, while policy denials
remain binding. Writes use the byte-preserving VFS channel and materialize to
disk; there is no automatic undo. Revision checks detect stale buffers but are
not an atomic compare-and-swap against other processes; coordinate concurrent
writers. Agent-staged changes still use the existing TUI Review changes screen.

Reads fail instead of truncating editable text above 1 MiB or accepting non-UTF-8
files. Context accepts 1–32 explicit paths and at most 4 MiB total; search is
literal and case-sensitive over selected files, with one-based human line/column
locations and an explicit truncation flag at 1,000 matching lines. Directory
listings retain the filesystem service's 500-entry limit/truncation flag.
Use `--` before filenames or queries that start with a dash.

Language queries currently support Rust, matching the shared backend's
`rust-analyzer` integration. Install that executable on PATH. Navigation accepts
zero-based line and UTF-16 character offsets. Each command opens its document in
a fresh client and reaps that owned process on completion, failure or cancellation.
A diagnostic response with `state: pending` means no publication arrived within
15 seconds; it is not a clean result. The complete query is bounded to 60 seconds,
followed by bounded process cleanup. Other languages and missing tooling produce
explicit errors. Returned edits/completions are inspection data; no LSP code action
is executed or automatically applied.

In the TUI, choose **Workspace files and context** from F1/Ctrl+P, or press `b`
in command mode. Enter opens a directory/file, `/` filters filenames, left/backspace
goes up, and `r` refreshes. File previews support arrows, Home/End and PageUp/PageDown.
Press `a` to insert the previewed snapshot (including its path and revision) into
your existing chat draft; it is not sent until you submit. Preview/context files
are capped at 64 KiB and combined drafts at 128 KiB. Escape returns or cancels a
pending load. Use the CLI export/import workflow for editing the selected file.
