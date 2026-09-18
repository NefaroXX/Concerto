# Outstanding Work — Score Accordion V2 + Signature Text Motion

> Reference doc (not an ADR). Date: 2026-09-18. Branch: `feat/score-accordion-thinking`, base `4654109` (+ unstaged batch).

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
- **OSC title broadcaster** — Working/Thinking/Done + macOS flicker config flag.
- **CLI theme bridge** — 4 palettes → ANSI sets.
- **Desktop policy rail** — `‖ ok` bar-line provenance.
- **CLI rail suffix + fence hardening** — multi-line fence edge cases in single-line reveal.
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

- Coordinator milestones (all-Detail runs still lack Headline).
- Agent-ID case fragility (string equality).
- Mute persistence across restarts.

## CI Must-Prove (Linux)

- `cargo test -p concerto-cli --lib` 183/183 (incl. `reveal_line` settled assert fix).
- `cargo test -p concerto-desktop --lib views::chat` 52/52.
- Full desktop suite (9 ENV `SchemaMismatch` + read-only fails locally).
- Full CLI suite (183 tests).
- `cargo clippy --workspace --all-targets`.
- `cargo deny check`.
- WASM plugin tests.

## Pre-existing / Out-of-Scope

- `tools/process.rs` clippy unused imports.
- Config schema: 8 vs branch 7 global mismatch.
- Iced test-profile 10-20 min wall.
