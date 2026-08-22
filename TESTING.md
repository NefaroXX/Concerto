# Concerto test report

Use one copy of this sheet per build, operating system, and provider/model
combination. Mark each result **Pass**, **Fail**, **Blocked**, or **Not tested**.
Attach sanitized logs and screenshots for failures. Never include credentials or
private source content.

> Live-test forms: `docs/live-test-template.md` (generic template) and
> ready-made copies such as `docs/live-test-skills-mcp.md` (Skills + MCP,
> ADR-43 / PR #111).

## Test environment

| Field | Value |
|---|---|
| Date/time and timezone | |
| Tester | |
| Concerto commit | |
| Build type (`debug`/`release`) | |
| Operating system/version | |
| Desktop environment/terminal | |
| Frontend (`desktop`/`CLI`) | |
| Project path | |
| Provider and model | |
| Per-agent assignments | |
| Selected shell profile | |
| Multi-agent relationships changed? | |
| Policy rules/preset | |
| Memory enabled and TTL | |

## Automated workspace checks

CI uses Rust 1.96.0 and sets `CONCERTO_TEST_MODE=1`. The `CARGO_BUILD_JOBS=2` in the commands below caps build parallelism; CI does
not currently set that variable.

```bash
rustup target add --toolchain 1.96.0 wasm32-wasip2
cargo +1.96.0 fmt --all -- --check
cargo +1.96.0 clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo +1.96.0 build --workspace
CONCERTO_TEST_MODE=1 CARGO_BUILD_JOBS=2 cargo +1.96.0 nextest run --workspace
CONCERTO_TEST_MODE=1 cargo +1.96.0 test --workspace --doc
cargo deny check
```

On PowerShell, set environment variables before the commands:

```powershell
$env:CARGO_BUILD_JOBS = "2"
$env:CONCERTO_TEST_MODE = "1"
```

| Check | Result | Notes/log reference |
|---|---|---|
| Formatting | | |
| Clippy, all targets | | |
| Workspace build | | |
| Nextest workspace tests | | |
| Documentation tests | | |
| Cargo deny | | |

### Acceptance-bar automation (Phase 6, 2026-08-01)

The audit's 12 end-to-end scenarios are the acceptance bar. Phase 6 closed the
G1-G5 gaps with automated tests (commits `ca0a35e`, `e0651ac`), so the
following scenarios no longer require fully manual execution:

- Scenario 2 (project switch never reuses the previous project's memory
  services): `select_or_init_memory_services` extraction + 4 project-switch
  tests (`crates/orchestrator/src/runtime_runner.rs`).
- Scenario 4 (compaction reduces model-input tokens):
  `compaction_reduces_model_input_tokens` and
  `compaction_does_not_lose_user_visible_turns`
  (`crates/orchestrator/src/context_compaction.rs`).
- Scenario 5 (provider failure after a file write does not repeat the write):
  `provider_failure_after_file_write_does_not_repeat_the_write`
  (`crates/orchestrator/src/agent_loop.rs`).
- Scenario 7 (continue restores the identical graph/artifacts/timestamps):
  `restore_yields_semantically_identical_state`
  (`crates/orchestrator/src/checkpoint.rs`).
- Scenario 8 (a Coder that creates only `Cargo.toml` cannot pass acceptance):
  `real_disk_build_cycle_rejects_placeholder_and_accepts_real_artifact`
  (`crates/orchestrator/src/coordinator.rs`).
- Scenario 12 (full gate incl. doctests): automated by CI — the test job in
  `.github/workflows/ci.yml` runs `cargo test --workspace` (doctests
  included) and it passes locally (exit 0). Doctests emit pre-existing `E0602 unknown lint:
  rustdoc::missing_docs` warnings from `[workspace.lints.rustdoc]` — non-fatal
  (the CI test job does not set `RUSTFLAGS="-D warnings"`), affects all crates
  uniformly, out of scope.

Scenarios not listed above are covered by tests from earlier phases or remain
manual per the sheets below.

## Default desktop checks

| Check | Expected result | Result/notes |
|---|---|---|
| First launch | App opens without panic; setup clearly requests missing provider details | |
| Project selection | Selected root is displayed and remains inside the intended project | |
| Provider test | Configured provider reports a useful success or sanitized error | |
| Model selection | Active provider/model labels match the selection after navigation and restart | |
| Empty chat | Quick actions seed (but do not submit) the composer; recent sessions resume on click | |
| Quick panel | Run/provider/model, agent assignments, Git state, and active view are accurate | |
| Quick-panel views | Chat, Studio, and Terminal buttons navigate to working views | |
| Chat, single-agent | A normal text reply streams and completes without tool activity | |
| Plan, single-agent | A plan reply completes; no tools run and no files change | |
| Build, single-agent | Agent creates/edits the requested files and exposes a reviewable diff | |
| Diff review | Accept/reject works per hunk and rejected content is not committed | |
| Tool log | Calls, outcomes, durations, and useful failure details appear | |
| Terminal | Starts the selected shell in the configured working directory | |
| Screenshot | Capture completes or reports a descriptive platform error | |
| Restart | Project, UI settings, provider/model intent, and shell selection persist | |

## Multi-agent Chat and Plan

Run both Chat and Plan with multi-agent mode enabled.

| Check | Expected result | Result/notes |
|---|---|---|
| Coordinator ownership | Only Coordinator responds; specialists are not dispatched | |
| Tool isolation | No filesystem or shell calls run | |
| Project integrity | No file is created, changed, or deleted | |
| Model assignment | Coordinator uses its assigned provider/model, if explicitly assigned | |

## Multi-agent Build and dependency order

Use a small project with a task that requires design, implementation, review,
and validation. Record the visible dispatch order.

| Check | Expected result | Result/notes |
|---|---|---|
| Task graph | Coordinator creates specialist work with meaningful dependencies | |
| Ready-task scheduling | Independent tasks may overlap; dependent tasks wait | |
| Coder prerequisite | Reviewer/Validator do not judge missing code before Coder output exists | |
| Research handoff | Research output is available to the dependent Coder task | |
| File modification | Coder uses filesystem tools and at least one intended file changes | |
| Review repair | Actionable review/validation feedback can return to Coder within cycle limits | |
| Partial progress | Exhausted recoverable work reports Blocked/Partial and preserves useful changes | |
| Provider identity | Each role uses its explicit provider/model assignment | |
| Spend | Multi-agent calls contribute to the same session totals and cap | |
| Phase timeline | Each dispatched role has one structured state row; phase events are not duplicated as noisy thinking blocks | |
| Tool timeline | Coder tool calls show success/failure details; a failed tool does not remain marked running | |
| Completion card | Completion/partial state and file chips agree with the authoritative run output | |

## Orchestration Studio

Use an existing valid multi-agent configuration, then make and save one small
change. Restart Concerto and verify the saved value before restoring it.

| Check | Expected result | Result/notes |
|---|---|---|
| Open and idle | Studio opens promptly and does not cause sustained CPU usage, input lag, or unbounded layout growth | |
| Agent navigation | Search filters agents; selecting an agent opens Prompt/Model/Permissions; Pipeline returns to the overview | |
| Relationship editor | Add, Edit, Cancel/Clear, and Delete affect the intended relationship and show readable cycle limits | |
| Invalid relationship | Self-links, duplicates, zero/invalid cycle limits, unknown endpoints, and dependency cycles are blocked before mutation | |
| Standard reset | Loading the "Standard Pipeline" preset from the toolbar's "Load preset…" pick-list (the single reset entry point) produces a valid acyclic pipeline | |
| Provider/model assignment | Explicit assignment is visible and persisted; Use global default removes the explicit assignment | |
| Deferred persistence | Editing shows Unsaved changes but does not write until Save; Save is disabled while validation fails | |
| Save feedback | Successful save shows Saved; a write failure remains unsaved and displays a useful error | |
| Restart persistence | Saved prompts, permissions, model assignments, agents, and relationships reload accurately | |

## Retry recovery

During streaming generation, briefly interrupt connectivity and restore it
before any configured `max_elapsed_seconds` fuse expires.

| Check | Expected result | Result/notes |
|---|---|---|
| Retry visibility | UI shows a retry/backoff state rather than generic `INTERNAL_ERROR` | |
| Same-run resume | Restoring connectivity continues the same run/session | |
| Context preserved | Existing task graph, tool results, and partial changes remain available | |
| Exhaustion | If retries expire, the result is actionable and partial progress remains | |

Do not use a destructive network test on an unsaved project. A local provider
or controlled firewall rule is preferable.

## Cancellation

| Check | Expected result | Result/notes |
|---|---|---|
| Cancel during generation | Work stops promptly and a neutral cancellation message appears | |
| Cancel during retry backoff | Pending retry stops and no later provider call is made | |
| Process cleanup | Running shell child/process group is terminated | |
| Restart after cancel | A new run can start without restarting Concerto | |

## Memory restart and projects

| Check | Expected result | Result/notes |
|---|---|---|
| Cold Memory view | After restart, open Memory before a run; the view loads without panic | |
| Re-index | Re-index completes or reports a useful error; indexed counts update | |
| File watcher | Changing an indexed file queues/reflects updated content | |
| Project switch | Results from project A do not appear in project B | |
| Switch back | Project A memory is still available after returning | |
| First embedding use | Model download/load is visible enough to distinguish it from a hang | |

## Spend chip and Spend Log

| Check | Expected result | Result/notes |
|---|---|---|
| Status-bar spend chip | Live session spend appears in the status-bar chip after real calls | |
| Warning threshold | Chip uses the warning palette color at ≥80% of the session cap | |
| Cap threshold | Chip uses the danger palette color at ≥100% of the session cap | |
| Spend Log modal | Spend Log opens over Chat and lists persisted per-call spend records | |
| Restart persistence | Per-call spend records and the chip totals remain after restart | |
| Multi-agent totals | Specialist calls are recorded once, not omitted or double-counted | |
| Daily-total stub | Daily-total output is present but empty until daily tracking is enabled | |

## Shell and policy

| Check | Expected result | Result/notes |
|---|---|---|
| Detected shells | Only installed/detected shells and explicit custom profiles appear | |
| Add profile | A valid custom executable can be added, verified, selected, and persisted | |
| Unified selection | Agents, Validator commands, and integrated terminal use the selected profile | |
| Missing executable | Concerto reports the missing path and remains usable | |
| Rule preview | Policy editor describes the selected action and exact condition | |
| First match | Earlier matching rule wins; reordering changes the outcome predictably | |
| Filesystem operations | `read`, `write`, `delete`, and `exists` rules match their named operations | |
| Path glob | `src/**` and `**/*.rs` match project-relative paths as described | |
| Command regex | A valid expression matches; an invalid expression is rejected/does not match | |
| Shell denylist | A hard-denied command remains denied even with no policy rules/expert mode | |

## Skills and MCP (Linux desktop, ADR-43)

| Check | Expected result | Result/notes |
|---|---|---|
| Skills discovery | With `[skills] enabled = true` and a valid `skill.toml`/`SKILL.md` pack on `search_paths`, Settings lists the pack; `concerto extensions list` shows it too | |
| Prompt injection | With the skill enabled (or `auto_load = true`), the running agent's prompt contains a `## Skills` section with the pack's instructions; a pack exceeding `max_chars` is truncated with the marker | |
| Skills never execute | Enabling a skill adds no tools to the registry; no executable behavior changes | |
| MCP server start | With `[mcp] enabled = true` and a fixture-mcp-server entry, Settings probe shows the server state and its `echo`/`fail`/`slow`/`crash` tools | |
| Tool naming | Tools appear as `mcp:<server_id>:<tool_name>` in Settings, Tool Log, and approval prompts | |
| Policy default | An MCP tool call is not auto-approved: approval is requested before execution | |
| Policy override | An explicit `tool_name_prefix = "mcp:<server_id>:"` `auto_approve` rule placed before the default makes that server's tools run without approval | |
| Approve + run | Approving `mcp:...:echo` returns the server's payload as the tool result | |
| Tool timeout | `mcp:...:slow` with `timeout_secs` under the hard cap returns a timeout tool error, not a hang | |
| Crash recovery | Killing the fixture server (or `FIXTURE_CRASH_ON_START=1`) clears its tools and shows a state-change notice; the app stays usable and the server can be restarted | |
| Config v1 semantics | Changes made in Settings → Skills/MCP take effect on the next run; add/remove of servers happens in the config file | |

## Test project result

| Field | Value |
|---|---|
| Task/prompt | |
| Expected files/behavior | |
| Files actually created/changed | |
| Build/test command | |
| Build/test result | |
| Functional checks performed | |
| Concerto final status/message | |
| Defects found | |

## Failure report

For each failure, record:

1. the check name and expected behavior;
2. exact reproduction steps;
3. whether the same task works with multi-agent disabled;
4. the visible agent, provider/model, and last successful event;
5. the full sanitized error and relevant console/log excerpt;
6. whether retry, cancel, or a new run worked without restarting the app;
7. files changed before failure and whether they were preserved;
8. frequency (`always`, `intermittent`, or `once`).

Severity guide:

- **Critical:** credential exposure, destructive escape, or unrecoverable data loss.
- **High:** normal use is blocked and there is no in-app recovery.
- **Medium:** a feature fails but another workflow remains usable.
- **Low:** confusing copy, layout, inaccurate status, or minor inconvenience.
