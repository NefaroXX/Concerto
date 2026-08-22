## Description

Briefly describe what this PR does and why.

## Related issues / ADRs

Closes #(issue)

<!-- If this changes cross-crate ownership, a persistent format, a security
     boundary, or a product-wide invariant, link the governing ADR
     (docs/adrs/ADR-NN.md). Superseding an ADR? Link both directions. -->

ADR: docs/adrs/

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor / chore
- [ ] Documentation
- [ ] Tests only

## Testing performed

Run locally against Rust 1.96.0 (the pinned CI toolchain):

- [ ] `rustup target add --toolchain 1.96.0 wasm32-wasip2`
- [ ] `cargo +1.96.0 fmt --all -- --check`
- [ ] `cargo +1.96.0 clippy --workspace --all-targets -- -D warnings`
- [ ] `CONCERTO_TEST_MODE=1 cargo +1.96.0 test --workspace`
- [ ] `cargo deny check`

New tests added or updated:

- [ ] Yes — covers both the contract and at least one failure case
      (each `#[test]` cites what it verifies)
- [ ] No behavioral change to test

## Code expectations

- [ ] No `unwrap()`/`expect()` in library paths (`expect` only in binary
      startup for mandatory config, with an explicit invariant message)
- [ ] `CancellationToken` threaded through any new long-running async work,
      and the cancelled path is tested
- [ ] Policy checks stay centralized in `ToolExecutor`; no bypassing of
      `SimplePolicyEngine`, shell validation, or `VirtualFs`
- [ ] Recoverable provider/tool/model problems are classified as data or
      blocked/partial outcomes, not generic terminal failures
- [ ] New dependencies have a concrete, justified need
- [ ] Desktop view/UI colors come from the theme palette
      (`scripts/check-hardcoded-colors.sh` passes)

## Screenshots (desktop UI changes)

<!-- Before/after screenshots for any change to crates/desktop views or ui. -->

## Changelog

- [ ] `CHANGELOG.md` updated under `[Unreleased]` (if user-facing)

## Notes for reviewers

<!-- Anything specific you'd like reviewers to focus on. -->
