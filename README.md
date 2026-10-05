# Concerto

A pre-release, local-first AI coding agent harness written in Rust.
Concerto runs single-agent loops and gated multi-agent orchestration entirely
on your machine against your choice of LLM providers: every model-generated
file write, shell command, and git operation passes through a policy engine
into a reversible filesystem overlay, and every decision is recorded on an
append-only audit trail. Native Iced desktop and independent ratatui terminal
frontends share one runtime, one configuration model, and one persistent
project memory.

Version 0.1.0, source builds only, nothing published yet — this README
describes what is actually implemented. For what is still open, see
[Honest boundaries](#honest-boundaries) and the 24-row
[deferred register](docs/DEFERRED.md).

[![CI](https://github.com/NefaroXX/Concerto/actions/workflows/ci.yml/badge.svg)](https://github.com/NefaroXX/Concerto/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)
![Rust](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)
[![Docs](https://img.shields.io/badge/docs-architecture-blue)](docs/architecture.md)
[![ADRs](https://img.shields.io/badge/design-ADRs-blueviolet)](docs/adrs/README.md)

> **Release status:** pre-release (0.1.0) entering wider live testing. Source
> builds and the automated workspace checks are the supported distribution
> path; nothing is published to crates.io yet. Read
> [Current Status](docs/STATUS.md) and [Testing](TESTING.md) before reporting a
> result.

## Origins

Concerto is a Rust reimplementation inspired by
[OpenCode](https://github.com/sst/opencode), whose fingerprints are still on
the architecture. The core idea this project is built around — one unified agent
loop where the model picks its own tools and acts on tool feedback, governed by
per-tool permission rules instead of pre-classifying the request — comes from
OpenCode's single-loop design together with Anthropic's *"Building Effective
Agents"* augmented-LLM pattern; ADR-55
([docs/adrs/ADR-55-intent-routing-and-authorization.md](docs/adrs/ADR-55-intent-routing-and-authorization.md))
credits both.

What diverged is everything around the loop: a first-match policy engine whose
unmatched verdict is *deny*, not ask; a `VirtualFs` overlay that preserves
originals for review or rejection; a supervised multi-agent coordinator with a
delegation doctrine instead of a flat agent list; WASM plugins; an append-only
audit trail; and SQLite-backed project memory.

Concerto is not a fork of OpenCode and depends on no OpenCode code — the
workspace has no `opencode` dependency and all of it is original. It would,
though, not have this shape without OpenCode.

*(A separate product with a similar name: **OpenCode Zen** is a hosted model
gateway, and Concerto talks to it as one provider among many via
`crates/providers/src/opencode.rs`. It is not the OpenCode agent discussed
here.)*

## Why Concerto?

Hosted coding agents ask you to sync your source code to someone else's
infrastructure and rely on prompt-level guardrails. Concerto keeps execution,
state, and authorization on your machine:

| Dimension | Typical cloud coding agent | Concerto |
|---|---|---|
| Code locality | Source synced to vendor runners | Local-first: code never leaves the machine |
| Tool authorization | Prompt-level guardrails | First-match policy engine; unmatched actions are denied |
| Reversibility | Post-hoc git repair | `VirtualFs` overlay preserves originals for review or rejection |
| Extensibility | Vendor plugin stores | WASM plugins (tools, providers, memory adapters) + MCP servers |
| State | Vendor-hosted history | SQLite sessions with replay primitives and spend tracking |
| Auditability | Opaque by default | Typed event bus feeding an append-only audit log |

## Features

**Two native frontends.** The `concerto-desktop` Iced 0.14 GUI provides chat,
a diff viewer, an in-app code editor, an integrated terminal panel, a memory
explorer and graph, an agent graph, settings, an orchestration studio, a tool
activity log, and a spend log. The `concerto-cli` ratatui 0.29 TUI provides an
independent chat, approvals, diff review, provider setup, and single/multi-agent
execution.

**Single- and multi-agent execution.** A streaming plan/act/observe loop with
bounded continuation, cancellation, cycle detection, and recoverable-error
handling. Every non-empty run enters the same unified loop; the Coordinator
(ADR-71) is the sole master of a run — it decides all dispatch, ordering, agent
selection, and termination, and the run shape is recorded for every run. The
five specialists (Architect, Researcher, Coder, Reviewer, Validator) are
config-driven seeds with dependency-aware task scheduling and per-role
provider/model assignments. Tools are granted per confirmed policy decision,
never by a chat/plan/build keyword: boundaries are enforced by deny-class
policy rules and the approval sink.

**Delegation doctrine (ADR-74).** Delegating is the default. The Coordinator
self-executes a subtask only when the roster is exhausted, and an attempt to do
so is refused by a named `delegation-required` policy rule and recorded as
`CoordinatorSelfImplementing`. A hard-failed subtask is first offered to another
registered specialist — an agent whose `can_cover` list includes the target
stage is eligible — and only then does the ladder escalate across providers. A
provider that is merely *throttled* is **held** (30 s × 2, three recovery
rounds) and retried rather than abandoned; the fallback pipe bridges the run
instead of demoting a rung that was only breathing. Agent coverage is
configuration, not code: `can_cover` in `[[multi_agent.custom_agents]]` widens
what an agent can do without a code change.

**Auditable tool trail.** Every file, shell, and git decision is written to an
append-only audit row recording the operation, the attempted and resolved
paths, the destinations, and whether it succeeded. Read-only operations record
what they returned — `exists`, entry counts, byte sizes, never content. Read a
session's trail with `concerto --cli audit <session-id>`, filtered by
`--tool`/`--operation`/`--failed`/`--limit`/`--json`.

**Evidence-gated acceptance (C-06).** A mutating run does not report Success on
a validator's say-so. It completes only on checkable evidence — a successful
mutating tool call against a declared deliverable, a successful build/test
command, or a recorded coordinator declaration. No evidence still reports
Partial rather than Success. (The remaining C-06 work is the *manual*
build-then-accept/reject cycle on disk, tracked in `docs/DEFERRED.md` row 34.)

**Provider support.** **23 provider ids** are registered in `PROVIDER_TYPE_IDS`
(`crates/providers/src/provider_defs.rs`): Anthropic, OpenAI, Google,
OpenRouter, NIM, Ollama, OpenCode Zen, OpenCode Zen (free), DeepSeek, Groq,
Together, Mistral, xAI, Fireworks, Cerebras, Cohere, DeepInfra, Perplexity,
SambaNova, DashScope, Moonshot, Zhipu, and Novita — all reachable from one
config-first catalog through a shared streaming interface with retry/backoff
and token metering. What is *not* here is the Tier-3 tier: GitHub Copilot,
Amazon Bedrock, Azure OpenAI, Google Vertex, and IBM watsonx are full agent
SDKs rather than API endpoints, and none has a wrapper (DEFERRED row 6;
[missing-providers.md](docs/missing-providers.md)).

**Policy governance.** Every file write, shell command, and git operation
passes through `SimplePolicyEngine` and the `VirtualFs` overlay — there is no
bypass. Rules are evaluated first-match; anything unmatched is **denied** by
the `default_deny` rule. (When MCP is enabled, the runtime appends an explicit
`RequireApproval` rule for `mcp:*` after your own rules, so unmatched MCP tools
ask rather than deny — an added rule, not the default.)
Filesystem changes are materialized immediately while the overlay
preserves original content for diff review and rejection. An approval request
**parks until it is answered** — the previous 30 s auto-expiry is inert, and
cancellation is the only escape. Every decision is recorded in an append-only
audit log.

**Sandboxing.** An opt-in containerized profile runs shell invocations through
`docker` or `podman` on Linux/macOS, with a fail-closed admission gate and a
required routing marker (ADR-72). Windows is not supported and fails closed.
WASM plugin capability enforcement is a second layer, not OS-level isolation.

**Persistence.** SQLite-backed sessions with audit log, replay primitives, and
shared token/USD spend tracking (`concerto-sessions`).

**Project memory.** SQLite FTS5 + vector hybrid retrieval with reciprocal-rank
fusion, local `fastembed` embeddings (first use may download model data),
tree-sitter AST chunking, project isolation, and a debounced file watcher for
re-indexing (`concerto-memory`). SQLite is the only vector-store backend.

**Extensibility.** WASM plugins for tools, providers, and memory adapters via
`concerto-plugin-sdk`, with manifest validation and capability grants
(TTL/hash pinning/revocation). Runtime discovery requires persisted approval;
plugin tools use `plugin:<id>:<tool>` names. Host file, shell, and HTTP effects
pass through shared policy, with file writes tracked in the shared `VirtualFs` for diff and undo.
MCP stdio servers (protocol `2025-11-25`)
expose tools namespaced `mcp:<server>:<tool>`, collision-checked and
policy-gated like any tool. Local Skills (`skill.toml` or `SKILL.md`
instruction packs) are injected into prompts by `SkillsContext` and never
execute code. Skills and MCP are disabled by default; enable them in
configuration.

**Security posture.** API keys live in the OS keychain, not in TOML files.
The API server binds loopback by default and refuses non-localhost binds
without `CONCERTO_API_KEY`. Logs sanitize secrets, and there is no built-in
telemetry (optional exporters are opt-in).

## Honest boundaries

What Concerto is not, stated plainly so nobody discovers it by surprise:

- **Pre-release.** Version 0.1.0, every crate marked `publish = false`, so
  nothing is on crates.io. A tag-triggered
  [release workflow](.github/workflows/release.yml) can build and attach a
  GitHub release, but no tagged release exists yet. The supported path today is
  a source build plus the workspace checks below. It is entering wider live
  testing, not finished testing.
- **One project at a time.** Concerto opens a single project directory per
  session. That is an accepted limitation, not a bug in progress
  (`docs/DEFERRED.md` row 16).
- **Sandboxing is Linux/macOS only.** The opt-in container profile routes
  shell invocations through `docker`/`podman` with a fail-closed admission
  gate; Windows has no container path and fails closed (row 36). WASM plugin
  capability enforcement is a second layer, not OS-level isolation.
- **The desktop UI is mid-polish.** Minimal and Medium rework stages are
  merged; the hybrid-UI finish work is still open (row 37).
- **No gateway layer.** No remote-execution gateway is built or scheduled;
  native direct connections are the default and the config-first catalog can
  already express a gateway as an OpenAI-compatible endpoint. Whether one is
  ever wanted is explicitly *undecided*, not refused (row 8).
- **Acceptance evidence is gated, acceptance itself is manual.** A mutating
  run cannot report Success without checkable evidence (a mutating tool call,
  a passing build/test command, or a recorded coordinator declaration), but
  the build-then-accept/reject cycle on disk is still yours (row 34).
- **macOS and Windows are not in the verified test matrix.** Linux is the
  primary development platform.

The bullets above are the highest-impact user-facing gaps. They are **not** the
whole picture: the register also carries Tier-3 provider SDKs, memory and
recall depth, eval breadth and flake quarantine, the plugin trust model and
marketplace, shell CPU limiting, provider transport resilience, catalog and
fallback controls, cross-process continue, and the unbuilt phases of the
AI-native shell. The full list — 24 rows, each with its size, its evidence, and
what unblocks it — lives in
[docs/DEFERRED.md](docs/DEFERRED.md), which is the source of truth for
"not done". This section is only a summary.

## Architecture

Concerto is a 25-crate Rust workspace. `concerto-core` is the foundation: it
owns cross-cutting contracts — events, IDs, provider/tool/policy traits,
cancellation, the `EventBus`, `SimplePolicyEngine`, and `ToolExecutor` — and
depends on no other workspace crate. Everything else builds upward from it:
providers and tools, the single-agent `AgentLoop` and multi-agent
`CoordinatorAgent` in the orchestrator, persistence (sessions, memory), the
desktop/CLI/API frontends, and the plugin/skills/MCP extension crates.

See [Architecture](docs/architecture.md) for runtime data flow and
[crate dependency graph](docs/crate-graph.md) for ownership and dependency
details.

## Screenshots

> Placeholder — screenshots of the desktop chat canvas, diff viewer,
> orchestration studio, and terminal UI will accompany the public release.
> Until then, build and run from source below; the desktop app is the default
> frontend and the CLI is one flag away.

## Prerequisites

Linux is the primary development platform; macOS and Windows are not yet part
of the verified test matrix.

- Rust 1.88 or newer (the workspace MSRV). CI, and `rust-toolchain.toml`,
  pin formatting, linting, and testing to Rust 1.96.0.
- The `wasm32-wasip2` Rust target — required to build the
  `test-*-plugin-wasm` crates in the workspace.
- A C toolchain and the platform development libraries used by SQLite, TLS,
  keyring, protobuf, and Iced/wgpu (X11/Wayland/GL/Vulkan on Linux).
- `cargo-deny` to reproduce the supply-chain job, and `cargo-nextest` only if
  you want the faster local test runner [TESTING.md](TESTING.md) uses.

CI installs exactly this system set on `ubuntu-latest`:

```bash
sudo apt-get install -y --no-install-recommends \
  libdbus-1-dev pkg-config libssl-dev libsqlite3-dev
```

A local full-GUI build additionally needs the X11/Wayland/Vulkan development
packages wgpu links against, plus a protobuf compiler. Package names vary by
distribution.

## Installation

Install from source:

```bash
git clone https://github.com/NefaroXX/Concerto.git
cd Concerto
rustup target add wasm32-wasip2
cargo build --workspace
```

Launch the desktop application:

```bash
cargo run -p concerto-desktop --release
```

Launch the terminal UI:

```bash
cargo run -p concerto-cli --release
```

The top-level binary includes both frontends by default. Desktop opens when no
command is supplied; explicit commands automatically select the CLI:

```bash
cargo run -p concerto -- --desktop
cargo run -p concerto -- --cli
cargo run -p concerto -- config keys
cargo run -p concerto -- config set retry.max_attempts 5
```

Fifteen subcommands are dispatched by `concerto-cli` — `audit`, `agents`,
`blueprint`, `config`, `credentials`, `extensions`, `health`, `logs`, `memory`,
`plugin`, `preferences`, `projects`, `providers`, `sessions`, and `shell` — so a
default build runs any of them without `--cli`; `--cli` forces the terminal UI.
On a build compiled with only the desktop feature, a subcommand such as
`concerto audit <id>` launches the GUI and ignores the arguments. Run
`concerto --help` for the per-subcommand action lists, or
[CLI settings and management commands](docs/cli-settings.md) for the full
reference.

For a terminal-only build, use `--no-default-features --features cli`.

## Quick start

On first launch, the setup flow (or the desktop Settings page) configures
providers. API keys are stored in the operating-system credential store, not
in the TOML configuration file. See [Provider and model
configuration](docs/models.md) and the [configuration
example](docs/config.toml.example) for details.

Select a project directory, choose a provider/model pair, and start with a
Chat or Build prompt. Multi-agent mode can be toggled in the desktop Settings
or enabled with `concerto --cli --multi-agent`.

Useful read-only checks (a default build selects the CLI for them; `--cli`
forces it):

```bash
concerto --cli health              # resolved provider stack, tier-1 default
concerto --cli config doctor       # config file, key presence
concerto --cli audit <session-id>  # one session's policy/tool audit trail
concerto --cli extensions list     # skills and MCP servers actually present
concerto --cli memory graph        # the project memory graph
```

`concerto --help` lists all fifteen subcommands and their actions.

## Configuration

Configuration is layered in this order:

1. built-in defaults;
2. the platform configuration file — `~/.config/concerto/config.toml` on Linux;
3. a project-root `.concerto.toml` file;
4. `CONCERTO_*` environment overrides (env always wins).

The `CONCERTO_` prefix is the only recognized environment override prefix.

The optional `project_roots` list restricts which project directories can be
opened. Set it as an array in the config file
(`project_roots = ["/path/to/project"]`) or via the `CONCERTO_PROJECT_ROOTS`
environment variable (platform-separated paths; the environment wins). When it
is non-empty, the desktop asks for consent before opening an out-of-root
project; when unset, behavior is permissive. The api-server reads
`CONCERTO_PROJECT_ROOTS` directly: a non-empty allowlist refuses out-of-root
session roots, and binding to a non-loopback address requires both
`CONCERTO_API_KEY` and a non-empty `CONCERTO_PROJECT_ROOTS`.

Useful guides:

- [CLI settings and management commands](docs/cli-settings.md)
- [Desktop/CLI capability parity and remaining work](docs/desktop-cli-parity.md)
- [Provider and model configuration](docs/models.md)
- [Multi-agent relationships](docs/agent-collaboration.md)
- [Policy rules](docs/policy-rules.md)
- [Shell profiles](docs/shell-profiles.md)
- [Native shell and security settings](docs/native-shell-security.md)
- [Skills](docs/skills.md) and [MCP servers](docs/mcp.md)
- [Configuration example](docs/config.toml.example)

### Use a new OpenAI-compatible endpoint (config-only)

Any OpenAI-compatible gateway — OpenRouter, NVIDIA NIM, OpenCode Zen, or a
self-hosted proxy — needs no code, only a `[[model_settings.providers]]` entry
(schema fields: `ProviderConfig` in `crates/config/src/schema.rs`):

```toml
[[model_settings.providers]]
id = "my-openai"
provider = "openai"                    # "opencode" for OpenCode Zen
model = "deepseek-r1"
api_base = "https://gateway.example.com/v1"
timeout_seconds = 60
keyring_key = "openai/api_key"
extra_models = ["other-model-1", "other-model-2"]   # optional
reasoning_echo = "always"              # "always" | "if-present"
# cache_breakpoints = true             # Anthropic providers only
```

- `extra_models` — extra model names this provider offers (resolution only).
- `reasoning_echo` (ADR-46) — DeepSeek-family gateways require
  `reasoning_content` (or `""`) on assistant messages once tool calls exist in
  history. `"always"` emits it on every assistant message (empty when none was
  captured); `"if-present"` (default) emits only captured reasoning; omit for
  the provider's built-in policy.
- `cache_breakpoints = true` — Anthropic only; marks system prompt + first
  user turn for prompt caching. No-op elsewhere.

**Key without the keychain.** `keyring_key` names an entry in the OS
credential store. At runtime, when no entry is stored, the provider factory
falls back to `<PROVIDER>_API_KEY`, with the provider type uppercased verbatim
(`crates/providers/src/factory.rs`): `OPENAI_API_KEY`, `OPENCODE_API_KEY`,
`NIM_API_KEY`, `OPENROUTER_API_KEY`. Note the one hyphenated id —
`provider = "opencode-free"` becomes `OPENCODE-FREE_API_KEY`, not
`OPENCODE_FREE_API_KEY`. Separately, with `CONCERTO_TEST_MODE=1`, lookups
derive env vars from `keyring_key` (uppercased, `/`/`-` → `_`):
`keyring_key = "openai/api_key"` → `CONCERTO_OPENAI_API_KEY`.

**Verify the setup:**

```bash
concerto --cli health        # resolved provider stack, tier-1 default
concerto --cli config doctor # config file, key presence
```

Example (`concerto --cli health`):

```
[my-openai] openai — model: deepseek-r1
    api base: https://gateway.example.com/v1
=== Tier-1 Default ===
  model: deepseek-r1 (served (tool-calling))
```

In multi-agent mode the fallback ladder re-dispatches a failed role on
`[multi_agent].default_model`, falling back to
`model_settings.global_default_model` when unset (see "Tier-1 Default" in
`concerto --cli health`).

## Testing and verification

The GitHub Actions workflow at `.github/workflows/ci.yml` is authoritative. It
runs on pushes and pull requests to `main` and `dev`, pins the toolchain to
1.96.0 via `rust-toolchain.toml`, and splits the checks into eight independent
jobs that deliberately declare no `needs:`, so one failure never masks another:

| Job | What it runs |
|---|---|
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `test` | `cargo test --workspace` (unit, integration, **and** doc tests) |
| `build` | `cargo build --workspace`, plus CLI smoke-runs and both single-frontend feature combinations |
| `bench` | compiles and smoke-runs every `[[bench]]` target with `--test` (no timing assertion) |
| `wasm-plugins` | builds the four `test-*-plugin-wasm` crates for `wasm32-wasip2` |
| `deny` | `cargo deny check` (licenses, bans, advisories — policy in `deny.toml`) |
| `ui-colors` | `scripts/check-hardcoded-colors.sh` — desktop `views/` and `ui/` must use `theme.palette.*`, never `Color::from_rgb`, `Color::{BLACK,WHITE,TRANSPARENT}`, or hex literals |

To reproduce them locally:

```bash
rustup target add wasm32-wasip2     # required before building the workspace
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONCERTO_TEST_MODE=1 cargo test --workspace
cargo deny check
```

- `CONCERTO_TEST_MODE=1` matches the `test` job. It is safe to omit: the only
  test that reaches the real keychain
  (`config::credentials::default_store_is_not_test_mode`) asserts constructor
  semantics without touching the backend, and test-mode credential access is
  otherwise selected per call via `CredentialStore::from_env()`. `TESTING.md`
  additionally uses `cargo nextest run --workspace` plus
  `cargo test --workspace --doc` as its faster local equivalent.
- Warnings are errors, but not via `RUSTFLAGS`: the `clippy` job passes
  `-D warnings` on the command line, and `[workspace.lints]` in `Cargo.toml`
  denies `clippy::all` and `unsafe_code` for every crate. Match that locally.
- Other workflows, not part of the PR gate: `bench-baseline.yml` (scheduled
  timing regression against a stored baseline), `dev-build.yml`,
  `native-shell.yml`, and the tag-triggered `release.yml`.

## Project layout

The Cargo workspace contains 25 crates, all under `crates/` (version 0.1.0,
edition 2021, `publish = false`, MIT OR Apache-2.0):

| Area | Crates |
|---|---|
| Foundation | `core`, `config`, `api-types` |
| Orchestration and execution | `orchestrator`, `providers`, `tools`, `shell`, `lsp` |
| Persistence and state | `sessions`, `memory` |
| Frontends | `desktop`, `cli`, `api-server`, `concerto` (entry binary) |
| Extensibility | `plugins`, `plugin-sdk`, `skills`, `mcp` |
| Evaluation and observability | `eval`, `eval-runner`, `observability` |
| WASM plugin examples | `test-plugin-wasm`, `test-provider-plugin-wasm`, `test-adapter-plugin-wasm`, `test-dialect-plugin-wasm` |

See [Architecture](docs/architecture.md) and the
[crate dependency graph](docs/crate-graph.md) for ownership and dependency
details.

## Documentation map

[docs/README.md](docs/README.md) is the index for everything under `docs/`.
Start with these:

- [Current Status](docs/STATUS.md) — implemented, maturing, and deferred scope
- [Deferred Work Register](docs/DEFERRED.md) — the single source of truth for
  "not done", with a re-entry condition per row
- [Testing](TESTING.md) — automated checks and tester report sheet
- [Roadmap](ROADMAP.md) — active priorities, not an assertion of completion
- [Architecture](docs/architecture.md) — runtime data flow and crate ownership
- [Crate graph](docs/crate-graph.md) — who depends on whom
- [Architecture Decision Records](docs/adrs/README.md) — numbered, append-only design history
- [Security Boundaries](SECURITY_BOUNDARIES.md) — enforced boundaries and gaps
- [Security threat model](docs/security-threat-model.md) — the analysis behind them
- [HTTP API schema](docs/api/openapi.json) — the api-server surface
- [Changelog](CHANGELOG.md) — released and unreleased changes
- [AGENTS.md](AGENTS.md) — the map AI coding agents are pointed at in this repo
- [TODO.md](docs/TODO.md) — unstarted work that has no deferral decision yet

## Contributing

Issues and test reports are especially valuable during this stage. Include the
Concerto commit, operating system, frontend, mode, provider/model assignments,
selected shell, policy summary, exact reproduction steps, and sanitized error
details. Do not include API keys or private source code.

- Development and review requirements: [CONTRIBUTING.md](CONTRIBUTING.md)
- Community conduct: [Code of Conduct](CODE_OF_CONDUCT.md)
- Security-sensitive findings: [SECURITY.md](SECURITY.md) (never in public issues)
- Issue templates: [bug report](.github/ISSUE_TEMPLATE/bug_report.yml),
  [feature request](.github/ISSUE_TEMPLATE/feature_request.yml),
  [question](.github/ISSUE_TEMPLATE/question.yml)

## Acknowledgements

Concerto stands on the Rust ecosystem — notably Tokio, SQLx, Axum, Iced,
ratatui, figment, wasmtime, tree-sitter, fastembed, gix, imara-diff, and
syntect — and follows the [Contributor Covenant](CODE_OF_CONDUCT.md) for
community conduct.

## Citation

If Concerto contributes to published research, please cite the repository:

```bibtex
@software{concerto,
  title  = {Concerto: A Local-First, Policy-Governed AI Coding Agent Harness},
  author = {NefaroXX},
  url    = {https://github.com/NefaroXX/Concerto},
  year   = {2026}
}
```

## License

Licensed under either:

- Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE)); or
- MIT ([LICENSE-MIT](LICENSE-MIT)).

at your option.
