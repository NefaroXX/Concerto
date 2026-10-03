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

## Extensions and shell

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
`enabled`, `env`, `timeout_secs`. Skill create/edit accepts the shared full
SkillManifest: id/name/version/description, optional instructions,
instructions_path, tools/resources. SKILL.md-only edits remain file-based;
removing a skill keeps recoverable manifest backups.

Install/enable operations do not turn on master subsystem switches. Plugin
install does not grant capabilities. MCP probes explicitly start the configured
program, list tools, then stop it without changing saved switches.
Choose a shell profile ID from `shell list`; detected IDs depend on the host.
CLI runs build a fresh plugin manager. Restart Desktop after changing installed
plugin files through these commands; its runtime manager is retained across runs.

## Appearance

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
