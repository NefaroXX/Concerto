# Concerto Live Test Form — Template

**Purpose**: Live verification of a specific change under real conditions.
Use one copy per feature/change, build type, OS, and provider/model
combination. Mark each result **Pass**, **Fail**, **Blocked**, or
**Not tested**. Attach sanitized logs and screenshots for failures. Never
include credentials or private source content.

## Test Environment

| Field | Value |
|---|---|
| Feature/Change Under Test | |
| Branch / PR | |
| Date/Time & Timezone | |
| Tester | |
| Concerto Commit/Tag | |
| Build Type | (Debug/Release) |
| OS/Version | |
| Frontend | (Desktop/CLI) |
| Provider & Model | |
| Shell Profile | |
| Policy Preset | |
| Memory Enabled (TTL) | |
| Configuration Highlights | (feature-relevant config used) |

## Key Tests

Fill in the feature-specific checks below (see the ready-made copies in
`docs/live-test-*.md` for worked examples).

| Check | Expected Result | Result/Notes |
|---|---|---|
| Primary flow | Describe the main user-facing behavior | |
| Edge cases & error handling | Describe failure behavior (no crash, clear message) | |
| Configuration & persistence | Describe config fields and restart behavior | |
| Policy/security gating | Describe approvals/denials and audit trail | |
| Tool execution & logging | Describe expected logs with outcomes | |
| Cancellation & recovery | Describe clean stop, partial progress preserved | |
| Spend & audit tracking | Describe per-role/per-tool metrics | |
| UX & feedback | Describe status indicators, errors, empty states | |
| Studio — auto-seed on first open | Open Orchestration Studio with no `[orchestration]`/custom agents → seeds appear (five specialists + standard blueprint) with no splash/init step; config file gains the sections idempotently | |
| Studio — roster CRUD → save → persist | Edit/add/delete an agent or stage → Save → restart → edits persist; coordinator row is locked (no edit/delete) | |
| Studio — deleted seed stays deleted | Delete a specialist (e.g. reviewer) → Save → restart → still absent, in the Studio and at runtime | |
| Studio — unknown stage kind | Enter a free-text stage kind → Save → reload → kind preserved, stage runs generically | |
| Studio — Settings → Relationships | Settings → Relationships hidden while `[orchestration]` is active | |
| Studio — validation surfacing | Break a rule (e.g. empty stage tag) → Save disabled + toolbar badge shows the issue | |

## Automated Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
CONCERTO_TEST_MODE=1 cargo nextest run --workspace
CONCERTO_TEST_MODE=1 cargo test --workspace --doc
cargo deny check
```

| Check | Result | Notes |
|---|---|---|
| cargo fmt --check | | |
| Clippy (-D warnings) | | |
| Workspace Build | | |
| Nextest + Doc Tests | | |
| Cargo Deny | | |

## Test Outcome

- Complex Task:
- Observations:
- Expected vs Actual:
- Build/Test Result:
- Final Status & Defects:

**Funding Notes**: Highlight task efficiency, revision success, cost control.
