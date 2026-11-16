# H40: headless history continuity

2026-11-16; isolated worktree `h40`, base `72c414a` (the published H39 head:
squash `2dbf25a` plus the ADR-87 acceptance commit).

H40 is implemented and its targeted checks pass; H41 is verified on its
supported platform. Neither is published yet — a full local CI battery is
running under separate `h40-final-*` logs, and its result is not claimed here.

## Why the clipped prose lost content

The headless path embedded conversation history into the submitted task
description as prose. The embedding clipped each message at 4,000 characters
and kept only the newest 16,000 characters in aggregate, so a constraint that
followed a long preamble and an older unresolved obligation both fell outside
the window, and the prior assistant turn arrived as user prose with no role.

The preserved request-level baseline
(`target/harness-reliability/h40-baseline-headless-history.log`) shows exactly
that shape: a system message plus one user message whose task text carried
`<conversation_history>`, with the long-preamble constraint and the older
obligation gone. Earlier fixture-failure logs are preserved as
`h40-baseline-cli.log`, `h40-baseline-cli-2.log` and `h40-baseline-cli-3.log`.

## Fixture-only harness defects (fixed)

The first executions of the fixture failed in the harness, not in production:

1. `--multi-agent` was placed after a `--project` startup pair, and that pair
   was stripped before `run` saw it. The fixture now puts `run` first and
   passes `--project` as a run option.
2. The loopback server answered with SSE only. The coordinator's weak-model
   transport is non-streamed (`stream: false`), so the provider rejected the
   body before history was observable. The server now answers `stream: false`
   with a JSON completion body.

## Production repair

`run_multi_agent` (`crates/orchestrator/src/runtime_runner.rs`) attaches typed
history whenever `approved_plan.is_none()`, regardless of `conversation_inbox`,
and the submitted task description stays the caller's objective. Approved-plan
runs still receive no conversation transcript: the whiteboard-verified
structured artifact and its decision ledger govern instead (ADR-60 D7), and the
D7 boundary has its own routing control.

The interactive inbox controls follow-up delivery and resume behavior
independently of initial history, so the repair does not manufacture an inbox
for a headless run merely to reach the history branch.

The clipped-prose `multi_agent_task_with_history` helper and the two
helper-only tests it fed were deleted, including the
`include!("history_continuity_regressions.rs")` probe file.

## Replacement coverage is request-level

- `crates/cli/tests/headless_history.rs` — the real headless CLI against a
  bounded loopback provider; asserts both durable user constraints, exactly one
  assistant-role prior reply, and that `<conversation_history>` is absent from
  the submitted task.
- `crates/orchestrator/src/coordinator/tests/coordinator_history_regressions.rs`
  — the same contract with and without an interactive inbox, wired into the
  coordinator test module.

Runtime fidelity is proven at request level, not by the helper replacement
alone.

The CLI fixture is Linux-only because its isolation relies on XDG application
directories. Do not broaden it to all Unix targets without a supported isolated
directory mechanism: macOS directory lookup can otherwise reach real user data.
The child also explicitly bypasses proxies for its loopback provider endpoint.

## Targeted evidence

In `target/harness-reliability/`: `h40-fix-headless-history.log` (CLI fixture
pass), `h40-orch-history-control.log` (coordinator control pass),
`h40-orch-d7-control-2.log` (D7 plus approved-plan routing control, 5/5),
`h40-orch-agency-topology.log` (`agency_topology` 9/9), `h40-fmt-2.log` (fmt
clean).

The first full local battery on `81fe15a` passed 11 of 12 gates in 55 m 32 s;
`audit` failed only on a crates.io yanked-package timeout. The failing log is
preserved as `h40-final-full-audit.first-attempt.log`, the resumed strict gate
passed (`h40-audit-resume-1.log`), and that attempt's logs are retained under
`h40-attempt1/`.

The final full battery on `8f2853e` passed **all 12 gates** in 34 m 02 s, and
all 1,019 source-snapshot hashes match. H40 and H41 are locally verified;
publication awaits the exact-head remote checks.

## H41: settings CLI fixture could access real macOS application data

Review found the same isolation mismatch in `tests/settings_commands.rs`: it
used `cfg(unix)` but redirected only XDG paths. The installed `dirs` 6.0.0
macOS implementation resolves config/data to the user's Library/Application
Support directory, and Concerto's config/session roots delegate to those
functions. The fixture runs mutating settings, agent and memory commands. This
establishes an unsafe test path by source inspection; no command was run
against a real profile to reproduce it.

The fixture is now restricted to Linux (`#![cfg(target_os = "linux")]`),
matching its actual isolation mechanism, and the existing Linux behavioral
tests are kept. `cargo test -p concerto-cli --test settings_commands` passes
11/11 on Linux (`h40-h41-settings-commands.log`). The gate is source-confirmed;
no mutating command was run against a real macOS profile, and portable fixture
coverage is not claimed. Portable CLI fixture coverage still needs an explicit
isolated application-directory mechanism; do not enable these tests on other
platforms merely by broadening the cfg.

The H40/H41 outcome is recorded in the main work log
(`docs/harness-reliability-work-log.md`).
