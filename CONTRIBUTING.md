# Contributing to Concerto

Concerto is pre-release. Focus contributions on reproducible defects, recovery,
cross-platform behavior, test coverage, and precise documentation rather than
expanding claims or adding speculative dependencies.

## Development setup

- Rust 1.88 or newer (the workspace MSRV). The repository pins Rust 1.96.0 via
  `rust-toolchain.toml`, so a rustup-managed installation resolves the exact CI
  toolchain automatically.
- The `wasm32-wasip2` target — required by the `test-*-plugin-wasm` crates.
- A C toolchain and platform development libraries (SQLite, TLS, keyring,
  protobuf, and Iced/wgpu — X11/Wayland/GL/Vulkan on Linux). On Debian/Ubuntu:
  `build-essential`, `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `clang`,
  `protobuf-compiler`.
- Node.js on `PATH` (verified at the start of every CI job).
- `cargo-nextest` and `cargo-deny` to reproduce all CI checks.

```bash
git clone https://github.com/NefaroXX/Concerto.git
cd Concerto
rustup target add wasm32-wasip2
cargo build --workspace
CONCERTO_TEST_MODE=1 cargo nextest run --workspace
```

## Looking for a first contribution?

Issues labeled [`good first issue`](https://github.com/NefaroXX/Concerto/labels/good%20first%20issue)
are scoped to be approachable without deep knowledge of the workspace. Ask
questions in a [Q&A discussion](../../discussions) before starting if the
issue leaves anything ambiguous.

## Before opening an issue

Use the report fields in [TESTING.md](TESTING.md). Include the commit, OS,
frontend, interaction mode, multi-agent state, provider/model assignments,
selected shell, policy summary, reproduction, and sanitized errors. State
whether a new run worked without restarting Concerto and whether partial files
were preserved.

Never include API keys, private project content, or unredacted prompts/logs in a
public report. Follow [SECURITY_BOUNDARIES.md](SECURITY_BOUNDARIES.md) for
security-sensitive findings.

## Branches, commits, and pull requests

- `main` is protected: never push to it directly. All changes land through
  pull requests.
- A pull request must be fmt/clippy/test/deny green locally before it is
  opened (CI re-verifies — see "Required checks" below). Keep PRs focused on
  one logical change.
- Use a short descriptive branch such as `fix/retry-resume`,
  `feat/policy-simulator`, `refactor/shell-profiles`, or `docs/test-guide`.
- Keep one logical change per commit.
- Use conventional commit subjects: `type(scope): summary`.
- Explain non-obvious motivation and trade-offs in the commit body.
- Preserve unrelated changes in a dirty worktree; do not use destructive reset
  commands to make a patch easier.
- Architectural changes require an ADR **before** the code that implements
  them (see [Architecture decisions](#architecture-decisions)).

## Required checks

The GitHub Actions workflow at `.github/workflows/ci.yml` is authoritative.
It runs fmt, clippy, test, and cargo-deny as separate, independent jobs and
pins the toolchain to Rust 1.96.0 through `rust-toolchain.toml`. The workspace
MSRV is Rust 1.88.

```bash
rustup target add wasm32-wasip2
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
CONCERTO_TEST_MODE=1 cargo nextest run --workspace
CONCERTO_TEST_MODE=1 cargo test --workspace --doc
cargo deny check
```

Run focused tests while iterating, then the relevant workspace checks before
opening a PR. `CONCERTO_TEST_MODE=1` prevents credential tests from touching
the real OS keychain.

## Code expectations

- No `unsafe` code; workspace lints deny it.
- Do not add `unwrap()`/`expect()` to library paths. A binary startup invariant
  may use an explicit `expect` message only when graceful recovery is impossible.
- Thread `CancellationToken` through long-running async work and test the
  cancelled path.
- Treat recoverable provider/tool/model problems as data or blocked/partial
  outcomes; do not turn them into a generic terminal failure.
- Keep policy checks centralized in `ToolExecutor`; do not bypass
  `SimplePolicyEngine`, shell validation, or `VirtualFs`.
- Desktop view colors must come from the theme palette. Run
  `scripts/check-hardcoded-colors.sh` when changing UI code.
- New dependencies require a concrete need and ownership justification. Remove
  unused dependencies instead of preserving an abandoned phase plan.

## Tests

Add the smallest test that proves the changed contract and at least one failure
case. For recovery changes, test classification, retry limit, cancellation, and
partial-state preservation. For configuration changes, test migration and
round-trip serialization. For shell changes, cover Windows and Unix behavior
where the code is platform-specific.

### Quality gates against useless tests (see `docs/concerto-test-quality-gates.md`)

- **Gate 2 — No test-count targets.** Never report "added N tests" as the
  primary justification. Report the specific (behavior, branch) pairs newly
  covered.
- **Gate 3 — Negative/adversarial cases.** Every function with a guard,
  `.is_empty()` check, checked arithmetic, or documented precondition must have a
  test that violates it and asserts correct (non-panicking) behavior.
- **Gate 4 — Tests must cite what they verify.** Every `#[test]` must have a
  doc comment or `// verifies:` line naming the specific behavior or bug it locks
  in.
- **Gate 5 — Regression tests before fixes.** When a bug is found, the first
  commit must be a failing test that reproduces it. Only the following commit may
  contain the fix.
- **Gate 7 — Independent adversarial review.** Before a "hardening" or "quality"
  branch merges, a differently-instructed reviewer (not the author) must attempt
  to break the changed code, not just run its test suite.

### Gate 8 — Spot-check mechanical edits

Any workspace-wide mechanical edit applied across ≥5 call sites must include
manual verification of a random sample of at least 3 sites in the PR description,
confirming the edit is functionally correct at each — e.g., for
`CancellationToken`, confirming it is actually raced via `select!`/checked via
`is_cancelled()`, not merely accepted as an unused parameter.

Update [TESTING.md](TESTING.md) when a change creates or alters a manual release
check.

## Architecture decisions

Use an ADR for a decision that changes cross-crate ownership, a persistent
format, a security boundary, or a product-wide invariant. ADR numbers are never
reused. A superseded ADR remains and points to its successor; historical context
is not rewritten to look current.

Small implementation choices belong in code/tests/PR rationale. The current ADR
index is in [ROADMAP.md](ROADMAP.md).

## Documentation

Executable sources and tests are authoritative. When behavior changes, update:

- `README.md` for user-facing promises;
- `docs/STATUS.md` for maturity/limitations;
- the relevant guide under `docs/`;
- `TESTING.md` for release verification;
- `CHANGELOG.md` under Unreleased.

Avoid “complete,” “production-ready,” or “all tests pass” unless the scope and
evidence are stated. Do not document planned functionality as shipped.
