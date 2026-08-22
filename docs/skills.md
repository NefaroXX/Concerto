# Skills

Skills are local, filesystem-based instruction packs (ADR-43, decision 1). A
skill is a directory with a manifest — either `skill.toml` or a `SKILL.md` file
with YAML front matter — plus optional instruction and resource files. Skills
**never execute code**; they are text that the orchestrator injects into prompts
so the model follows project- or team-specific conventions ("prefer cargo
nextest", "no `unwrap()` in library crates", a style guide).

Discovery, parsing, and loading live in `concerto-skills`
(`crates/skills/`). Injection and budgeting live in the orchestrator
(`crates/orchestrator/src/skills_context.rs`, `SkillsContext`).

## Pack layout

```
~/.local/share/concerto/skills/
└── rust-testing/              # one directory per skill pack
    ├── skill.toml             # manifest (or SKILL.md — skill.toml wins)
    ├── instructions.md        # optional; referenced by instructions_path
    └── fixtures/sample.rs     # optional resource files
```

Discovery (`SkillManager::discover`) walks each search path to a bounded depth
of 4 levels below the search-path root (the root is depth 0; a pack at depth 4
is found, depth 5 is not). A leading `~` in a search path expands to the user's
home directory (`dirs::home_dir`). Directories without a manifest are silently
not skills; missing search paths are warned about and skipped. A malformed
manifest fails loudly with the offending path. Results are sorted by id;
duplicate ids keep the first occurrence deterministically (lowest `(id, path)`
wins) and are warned about.

## Manifest formats

### `skill.toml`

Only `id` is required (non-empty after trimming). Optional fields default to
`""` (`name`, `version`, `description`), `None` (`instructions`,
`instructions_path`), or an empty list (`tools`, `resources`). Unknown fields
are ignored. `instructions` is inline text; `instructions_path` is a path
relative to the pack directory and wins over inline `instructions` when both
are set. Paths are resolved to absolute form in the loaded descriptor.

```toml
id = "rust-testing"
name = "Rust Testing"
version = "1.0.0"
description = "Cargo verification guidance"
instructions = "Prefer cargo nextest."
tools = ["cargo nextest run", "cargo clippy"]
resources = ["fixtures/sample.rs"]
```

### `SKILL.md`

YAML-subset front matter between `---` delimiter lines at the start of the
file; the markdown body below the closing `---` is the instruction text. A
front-matter `instructions` key is used only when the body is empty. When both
formats are present in one directory, `skill.toml` wins.

```markdown
---
id: commit-style
name: Commit Style
tools:
  - cargo test
  - cargo fmt
---
# Commit Style

Always use conventional commits.
```

The front matter is parsed by a small built-in parser (not a general YAML
implementation — `serde_yaml`/`yaml-rust` are deprecated). Supported subset:
scalar `key: value` lines for `id`, `name`, `version`, `description`,
`instructions`; list blocks for `tools` and `resources`; single-/double-quoted
and unquoted scalars; `#` comments. Not supported (an error): nested
structures, flow lists (`[a, b]`), block scalars (`|`, `>`), anchors/aliases,
escapes, multi-document `---`. Malformed front matter yields
`SkillsError::FrontMatter` with the file path and a reason.

## Configuration

The `[skills]` section (schema v5, ADR-43). Defaults apply when the section is
omitted.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Master switch (ADR-43 decision 5: explicit opt-in) |
| `search_paths` | `["~/.local/share/concerto/skills", "./.concerto/skills"]` | Directories to walk for packs |
| `auto_load` | `true` | When `enabled_ids` is unset, load every discovered skill |
| `enabled_ids` | `None` | Explicit allow-list of skill ids; `None` = allow-all |
| `max_chars` | `None` → 4000 | Hard character budget for the injected section |

```toml
[skills]
enabled = true
search_paths = ["~/.local/share/concerto/skills", "./.concerto/skills"]
auto_load = true
# enabled_ids = ["rust-testing", "commit-style"]   # omitted = allow-all
# max_chars = 4000
```

### Enable semantics

The effective load rule is resolved by `SkillsContext::refresh`:

- `enabled = false` (or section omitted) — fully disabled: no discovery runs
  and the prompt section is empty.
- `enabled_ids = Some(ids)` — only those ids load, in caller order; unknown ids
  are warned about and skipped.
- `enabled_ids = None` + `auto_load = true` — every discovered skill loads.
- `enabled_ids = None` + `auto_load = false` — nothing loads. An explicit
  **empty** allow-list (`Some([])`) with `auto_load` off skips discovery
  entirely, so a disabled configuration performs no filesystem work.

## Prompt injection, budget, and trust

`SkillsContext` formats the enabled packs into one markdown section (header
`## Skills`) and serves it to every prompt path — the single-agent
`PromptBuilder` and the coordinator specialist assembly. Formatting happens
once per `refresh`; the prompt hot path only clones the cached string.

- **Budget:** the section is capped at `max_chars` characters (orchestrator
  default 4000). Whole skill blocks are kept in deterministic id order while
  they fit; the first overflowing block is truncated into the remaining budget;
  a `[truncated: skill instructions exceed the session budget]` marker is
  appended when anything was cut. The output is always `<= max_chars`.
- **Fail-soft:** a discovery error keeps the previous section, is logged at
  error level, and never crashes the agent loop.
- **Trust:** skill instructions are text from disk injected into model prompts.
  Treat packs as trusted content — a pack can steer model behavior (including
  attempted prompt injection). Enable only packs you trust.
- **Tool hints only:** the `tools` list in a manifest is a prompt-level
  suggestion. It does not grant or strip tools; policy gating of actual tool
  execution is unchanged.

## Using skills

- **Config file:** edit `[skills]` in `config.toml` (or the project
  `.concerto.toml`) as above.
- **CLI:** `concerto extensions list` shows the skills section state — enabled,
  search paths, auto-load, enabled ids — and the packs discovered under the
  configured search paths. v1 is read-only; edits go through the config file.
- **Desktop:** Settings → Skills. v1 is config-driven: the enable toggle and
  per-skill checkboxes edit the *pending* config, persisted on Save Settings,
  and take effect on the next run. Search paths and auto-load are display-only.
  "Refresh" re-runs discovery for the current session (transient, not
  persisted). Until you tick a specific skill, discovery is in allow-all mode;
  an explicit check flips the section to an allow-list.

## Limitations

- Skills never execute code — they are instruction packs only (use WASM
  plugins for executable extensions).
- The `tools` list is a prompt-level suggestion; there is no automatic tool
  gating or stripping in v1.
- The desktop section is config-driven (next-run semantics); live toggles via a
  held runtime handle are deferred (ADR-43 §3 v1 note). CLI support is
  read-only.

See [ADR-43](adrs/ADR-43-skills-mcp-and-extension-manager.md) for the
architecture record and [architecture.md](architecture.md) for the crate
layout.
