# Outstanding Work — Score Accordion V2 + Signature Text Motion

> Reference doc (not an ADR). Date: 2026-09-16. Branch: `feat/score-accordion-thinking`, base `ad4b400`.

---

## Landed

- **V2 ThinkingKind buckets** + `/thinking` command + mute toggle.
- **Scan-line overlay** + wiring (default-off).
- **First-token emphasis** — desktop + CLI.
- **Handoff cue** — desktop + CLI.
- **Wipe** — desktop + CLI rule.
- **Startup wordmark / staff**.
- **[display] toggles** — `reduced_motion` + `scanline`.
- **CLI markdown-lite** + role gutter.
- Lint fixes across workspace.

## V3 Gaps (oracle)

- `transcript.rs` ThinkingEntry: needs `kind` field or `LowLevel` filter path.
- CLI collapse: digest empty for LowLevel-only agents (include `Detail` fallback).
- Coordinator: all-Detail run (no Headline milestones).
- `agent_id` string-equality case fragility.
- CLI `thought_log`: unbounded (needs trim).
- Mute: not persisted across restarts.
- `/thinking`: undiscoverable in desktop composer.
- Desktop digest: ignores LowLevel-only buckets.

## Text Roadmap Left

- OSC terminal-title broadcaster (Working/Thinking/Done + config flag for macOS flicker).
- CLI theme bridge (4 palettes → ANSI sets).
- Policy bar-line provenance rail (`‖ ok`).
- Multi-line fence edge cases in single-line reveal.

## CI Must-Prove (Linux)

- `cargo test -p concerto-cli --lib` 172/172 (incl. `reveal_line` settled assert fix).
- `cargo test -p concerto-desktop --lib views::chat` 52/52.
- Full desktop suite (9 ENV `SchemaMismatch` + read-only fails locally).
- `cargo clippy --workspace --all-targets`.
- `cargo deny check`.
- WASM plugin tests.

## Pre-existing / Out-of-Scope

- `tools/process.rs` clippy unused imports.
- Config schema: 8 vs branch 7 global mismatch.
- Iced test-profile 10-20 min wall.
