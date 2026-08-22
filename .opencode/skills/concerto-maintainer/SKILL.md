---
name: concerto-maintainer
description: Maintain the Concerto Rust workspace — CI, ADRs, docs, commits and releases. Use when editing ADRs, updating README/CONTRIBUTING, fixing CI, preparing releases, or handling GitHub community health files for Concerto.
---

# Concerto maintainer

Executable sources outrank these notes.

## Identity

Pre-release (0.1.0) local-first, policy-governed AI coding agent harness: **25-crate** pure-Rust workspace, every crate depending upward on `concerto-core`. Two frontends share one runtime/config: Iced 0.14 desktop + ratatui CLI. Write/shell/git ops pass policy gates into a reversible `VirtualFs`; audit log append-only. WASM plugins (tool/provider/memory/dialect), local skills (never execute code), stdio MCP servers. Maps: `docs/architecture.md`, `docs/crate-graph.md`.

## Environment

- MSRV 1.88; toolchain pinned 1.96.0 via `rust-toolchain.toml`; `wasm32-wasip2` target needed by the `test-*-plugin-wasm` crates.
- Ubuntu deps: libdbus-1-dev pkg-config libssl-dev libsqlite3-dev build-essential clang protobuf-compiler (+ graphics libs for desktop/wgpu); Node.js on PATH.
- cargo-nextest + cargo-deny; `CONCERTO_TEST_MODE=1` keeps tests off the real keychain.

## CI authority

`.github/workflows/ci.yml` is authoritative — parallel independent jobs: fmt, clippy, test, build, wasm-plugins, deny, ui-colors. Local equivalents:

```bash
rustup target add wasm32-wasip2
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
CONCERTO_TEST_MODE=1 cargo test --workspace   # plain cargo test incl. doctests
cargo deny check
bash scripts/check-hardcoded-colors.sh        # UI changes only
```

Tag `vX.Y.Z` → `.github/workflows/release.yml` (4 targets, auto release notes).

## Commits

- `main` protected: PRs only, from `fix/…`/`feat/…`/`refactor/…`/`docs/…` branches.
- Conventional `type(scope): summary`; atomic; body explains why.
- No unsafe; no `unwrap()`/`expect()` in library paths (binary startup invariants excepted, explicit message).
- `CancellationToken` through async work; test cancelled path.
- Dependencies need current justification; remove unused ones.
- ADR before code for ownership/format/security/invariant changes.

## ADRs (`docs/adrs/`)

1. Next free number — never reused (gaps stay).
2. `ADR-NN-slug.md` with Status + Date headers; real decision dates (founding set starts 2025-07-10); `Last updated:` footer on revision.
3. Add row to Active table in `docs/adrs/README.md`.
4. Superseded: full text → `archive/`, stub points to successor, index row moves to Archived table. Nothing deleted.

## Docs sync

Behavior changes update README (promises/badges/crate count), docs/STATUS.md, relevant docs/ guide, TESTING.md, CHANGELOG.md (Unreleased). Health files: SECURITY.md (GitHub Security tab), CODE_OF_CONDUCT.md, .github/SUPPORT.md, .github/pull_request_template.md.

## Checklists

**Failing CI** (first red gate wins):
1. `cargo fmt --all`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. Focused then workspace tests (`CONCERTO_TEST_MODE=1`)
4. Secret-scanning push block? Sanitizer vectors via `format!` splits (`format!("{}{}", "sk_live_", "…")`), never key-like literals.
5. `cargo deny check`

**New ADR**: lifecycle above; update ROADMAP links if cited.

**Stale-ref sweep** (gitea/mitservices): grep whole tree incl. `.github/` and comments; changelog history stays verbatim if true when written and no dead URL.

**Release prep**: CHANGELOG Unreleased → versioned; bump `[workspace.package] version`; check README badge/status; `chore(release)` commit; tag `vX.Y.Z`; refresh TESTING.md sheet.

## Gotchas

- `deny.toml` exceptions intentional: unmaintained transitive advisories ignored; wasmtime pinned v28 (v43+ needs func_wrap migration). Read before editing.
- No LanceDB — SqliteVectorStore only (archived ADR-10).
- Doctests emit known non-fatal missing_docs E0602 warnings; CI omits `-D warnings` there deliberately.
- Never bypass SimplePolicyEngine, ToolExecutor, VirtualFs.
- Desktop views/+ui/: colors from `theme.palette.*` only; widgets/ + theme/ exempt.
