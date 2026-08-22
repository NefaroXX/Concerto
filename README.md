# Concerto

A production-grade, local-first AI coding agent harness written in Rust.
Concerto runs single-agent loops and gated multi-agent orchestration entirely
on your machine against your choice of LLM providers: every model-generated
file write, shell command, and git operation passes through a policy engine
into a reversible filesystem overlay, and every decision is recorded on an
append-only audit trail. Native Iced desktop and independent ratatui terminal
frontends share one runtime, one configuration model, and one persistent
project memory.

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
a diff viewer, an integrated terminal panel, memory explorer, settings, an
orchestration studio, a tool activity log, and a spend log. The
`concerto-cli` ratatui TUI provides an independent chat, approvals, diff
review, provider setup, and single/multi-agent execution.

**Single- and multi-agent execution.** A streaming plan/act/observe loop with
bounded continuation, cancellation, cycle detection, and recoverable-error
handling. Multi-agent mode (a desktop toggle, or `--multi-agent` in the CLI)
runs a Coordinator plus five specialists — Architect, Researcher, Coder,
Reviewer, and Validator — with dependency-aware task scheduling, per-role
provider/model assignments, and policy-gated write access. Chat and Plan modes
never grant project tools; only Build does.

**Provider support.** OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama,
NVIDIA NIM, and OpenCode-compatible endpoints, through a shared streaming
interface with retry/backoff and token metering.

**Policy governance.** Every file write, shell command, and git operation
passes through `SimplePolicyEngine` and the `VirtualFs` overlay — there is no
bypass. Rules are evaluated first-match; unmatched tools default to requiring
approval. Filesystem changes are materialized immediately while the overlay
preserves original content for diff review and rejection. Every decision is
recorded in an append-only audit log.

**Persistence.** SQLite-backed sessions with audit log, replay primitives, and
shared token/USD spend tracking (`concerto-sessions`).

**Project memory.** SQLite FTS5 + vector hybrid retrieval with reciprocal-rank
fusion, local `fastembed` embeddings (first use may download model data),
tree-sitter AST chunking, project isolation, and a debounced file watcher for
re-indexing (`concerto-memory`). SQLite is the only vector-store backend.

**Extensibility.** WASM plugins for tools, providers, and memory adapters via
`concerto-plugin-sdk`, with manifest validation and capability grants
(TTL/hash pinning/revocation). MCP stdio servers (protocol `2025-11-25`)
expose tools namespaced `mcp:<server>:<tool>`, collision-checked and
policy-gated like any tool. Local Skills (`skill.toml` or `SKILL.md`
instruction packs) are injected into prompts by `SkillsContext` and never
execute code. Skills and MCP are disabled by default; enable them in
configuration.

**Security posture.** API keys live in the OS keychain, not in TOML files.
The API server binds loopback by default and refuses non-localhost binds
without `CONCERTO_API_KEY`. Logs sanitize secrets, and there is no built-in
telemetry (optional exporters are opt-in).

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

- Rust 1.88 or newer (the workspace MSRV). CI formats and lints with Rust
  1.96.0.
- The `wasm32-wasip2` Rust target — required to build the
  `test-*-plugin-wasm` crates in the workspace.
- A C toolchain and the platform development libraries used by SQLite, TLS,
  keyring, protobuf, and Iced/wgpu (X11/Wayland/GL/Vulkan on Linux).
- Node.js on `PATH` — CI verifies it at the start of every job.
- `cargo-nextest` and `cargo-deny` to reproduce all CI checks.

On Debian/Ubuntu, typical development packages include `build-essential`,
`pkg-config`, `libssl-dev`, `libsqlite3-dev`, `clang`, `protobuf-compiler`,
and the X11/Wayland/Vulkan development packages required by wgpu. Package
names vary by distribution.

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

The top-level binary also selects a frontend. Desktop is the default; the CLI
build is behind its feature:

```bash
cargo run -p concerto -- --desktop
cargo run -p concerto --features cli -- --cli
```

## Quick start

On first launch, the setup flow (or the desktop Settings page) configures
providers. API keys are stored in the operating-system credential store, not
in the TOML configuration file. See [Provider and model
configuration](docs/models.md) and the [configuration
example](docs/config.toml.example) for details.

Select a project directory, choose a provider/model pair, and start with a
Chat or Build prompt. Multi-agent mode can be toggled in the desktop Settings
or enabled with `concerto --cli --multi-agent`.

## Configuration

Configuration is layered in this order:

1. built-in defaults;
2. the platform configuration file — `~/.config/concerto/config.toml` on Linux
   (legacy `~/.config/opencode-rs/config.toml` locations are still recognized
   for migration);
3. a project-root `.concerto.toml` file;
4. `CONCERTO_*` environment overrides (env always wins).

Legacy `OPENCODE_RS_*` environment variables remain recognized for migration.
New configurations should use the `CONCERTO_` names and must not set both
prefixes for the same value.

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

- [Provider and model configuration](docs/models.md)
- [Multi-agent relationships](docs/agent-collaboration.md)
- [Policy rules](docs/policy-rules.md)
- [Shell profiles](docs/shell-profiles.md)
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
falls back to `<PROVIDER>_API_KEY` (provider type uppercased):
`OPENAI_API_KEY`, `OPENCODE_API_KEY`, `NIM_API_KEY`, `OPENROUTER_API_KEY`.
Separately, with `CONCERTO_TEST_MODE=1`, lookups derive env vars from
`keyring_key` (uppercased, `/`/`-` → `_`): `keyring_key = "openai/api_key"` →
`CONCERTO_OPENAI_API_KEY`, legacy alias `OPENCODE_RS_OPENAI_API_KEY`.

**Verify the setup:**

```bash
concerto health           # resolved provider stack, tier-1 default
concerto config doctor    # config file, key presence
```

Example (`concerto health`):

```
[my-openai] openai — model: deepseek-r1
    api base: https://gateway.example.com/v1
=== Tier-1 Default ===
  model: deepseek-r1 (served (tool-calling))
```

In multi-agent mode the fallback ladder re-dispatches a failed role on
`[multi_agent].default_model`, falling back to
`model_settings.global_default_model` when unset (see "Tier-1 Default" in
`concerto health`).

## Testing and verification

The GitHub Actions workflow at `.github/workflows/ci.yml` is authoritative;
it pins formatting, linting, and testing to Rust 1.96.0 via
`rust-toolchain.toml` and sets `RUSTFLAGS="-D warnings"`.

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CONCERTO_TEST_MODE=1 cargo test --workspace        # or: cargo nextest run --workspace
cargo test --workspace --doc
cargo deny check
```

- `CONCERTO_TEST_MODE=1` is the documented (but currently unused) switch for
  credential tests. Test-mode behavior is actually selected per-call via
  `CredentialStore::from_env()`; the one test asserting the production store
  is intentionally backend-agnostic so the suite stays green without a
  keychain. It is safe to omit it, but passing it costs nothing and matches
  CI.
- `rustup target add wasm32-wasip2` is required before building the workspace.
- Treat rustc warnings as errors locally too: the workspace lints deny
  `clippy::all` and `unsafe_code`.

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

- [Current Status](docs/STATUS.md) — implemented, maturing, and deferred scope
- [Testing](TESTING.md) — automated checks and tester report sheet
- [Roadmap](ROADMAP.md) — active priorities, not an assertion of completion
- [Architecture](docs/architecture.md) — runtime data flow and crate ownership
- [Architecture Decision Records](docs/adrs/README.md) — numbered, append-only design history
- [Security Boundaries](SECURITY_BOUNDARIES.md) — enforced boundaries and gaps
- [Changelog](CHANGELOG.md) — released and unreleased changes

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

Concerto stands on the Rust ecosystem — notably Tokio, Iced, ratatui, SQLx,
figment, wasmtime, tree-sitter, fastembed, and Axum — and follows the
[Contributor Covenant](CODE_OF_CONDUCT.md) for community conduct.

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
