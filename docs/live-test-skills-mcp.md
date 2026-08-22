# Concerto Live Test Form — Skills & MCP Extensions (ADR-43)

**Purpose**: Live verification of the Skills + MCP client + extension-manager
integration (ADR-43, `docs/skills.md`, `docs/mcp.md`). Covers skill discovery
and prompt injection, MCP stdio servers under policy, the desktop Settings
sections, and the CLI `extensions list` surface. Mark each result **Pass**,
**Fail**, **Blocked**, or **Not tested**. Attach sanitized logs/screenshots for
failures; never include credentials.

## Test Environment

| Field | Value |
|---|---|
| Feature/Change Under Test | Skills + MCP client + extension manager (ADR-43) |
| Branch / PR | `feat/skills-mcp-extensions` / PR #111 |
| Date/Time & Timezone | |
| Tester | |
| Concerto Commit/Tag | `3d53dd3` (HEAD of the PR branch) |
| Build Type | (Debug/Release) |
| OS/Version | |
| Frontend | Desktop **and** CLI |
| Provider & Model | |
| Shell Profile | |
| Policy Preset | Default (unmatched `mcp:*` → RequireApproval) |
| Memory Enabled (TTL) | |
| Multi-Agent Enabled | Optional — verifies skill injection into the coordinator path |
| Configuration Highlights | `[skills]` (`enabled`, `search_paths`, `auto_load`, `enabled_ids`, `max_chars`); `[mcp]` (`enabled`, per-server `id`/`command`/`args`/`env`/`enabled`/`timeout_secs`) — see `docs/config.toml.example` |

## Key Tests — Skills

| Check | Expected Result | Result/Notes |
|---|---|---|
| Discovery (desktop) | Settings → Skills section lists packs from `search_paths` (name, id, version, description, tool count); Refresh re-runs discovery; invalid/missing path shows a clear error, no crash | |
| Discovery (CLI) | `concerto extensions list` prints Skills section (enabled, search paths, auto-load, enabled ids) and MCP servers | |
| Default state | `skills.enabled = false` → nothing injected until enabled | |
| Enable semantics | Check a skill in Settings + Save → on the **next** message the instructions appear in the system context; unchecked skills are absent | |
| Allow-all vs allow-list | `enabled_ids` unset → all discovered packs injected; explicit list → only listed ids; empty list → none | |
| Injection paths | Instructions appear in BOTH single-agent runs and multi-agent coordinator runs | |
| Budget/truncation | Oversized instructions truncated with a clear marker under `max_chars` (default 4000) | |
| Preview | Skill row "Show instructions" renders the pack instructions read-only | |
| Persistence | Settings survive restart (pending config saved; toggles take effect next run by design) | |

## Key Tests — MCP

| Check | Expected Result | Result/Notes |
|---|---|---|
| Default posture | `mcp.enabled = false` → no servers start, no `mcp:` tools exist | |
| Fixture server | Configure `[[mcp.servers]]` id=`fixture`, command = your build of `fixture-mcp-server` (bin of concerto-mcp); tools appear namespaced `mcp:fixture:echo`, `mcp:fixture:fail`, `mcp:fixture:slow`, `mcp:fixture:crash` | |
| Policy deny | Unmatched `mcp:*` tool → RequireApproval prompt; deny → tool blocked, action audited | |
| Policy approve | Approve → tool runs; result returns to the agent | |
| Policy rule | Explicit rule (`condition: tool_name_prefix`, e.g. `mcp:fixture:echo`) → auto-approved; deny rule for `mcp:fixture:*` blocks the rest | |
| Tool outcomes | `echo` returns its payload; `fail` surfaces the server's `isError` result without crashing the loop | |
| Timeout | `slow` exceeds the per-call timeout → `ToolError::Timeout`, loop continues | |
| Crash handling | `crash` (or `FIXTURE_CRASH_ON_START=1`) → server state changes (`McpServerStateChanged` event), its tools become unavailable, agent loop survives | |
| Lifecycle | After quit/restart no orphan `fixture-mcp-server` processes remain; a fresh run re-registers tools | |
| Desktop probe | MCP Servers section: Test connection → "Connected — N tools" with names, or a danger-colored error line; per-server enable toggle + next-run note | |
| Config validation | Empty server `id` or one containing `:` → config load error; schema v5 `config.toml.example` fields all accepted | |

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
| Clippy (-D warnings) | | Expected: zero warnings (24 crates, toolchain 1.96.0) |
| Workspace Build | | |
| Nextest + Doc Tests | | Expected: 1998 passed / 0 failed (1 pre-existing `#[ignore]` eval test) |
| Cargo Deny | | Expected: pass; 2 benign `advisory-not-detected` notes |

## Test Outcome

- Complex Task:
- Observations:
- Expected vs Actual:
- Build/Test Result:
- Final Status & Defects:

**Funding Notes**: Highlight task efficiency, revision success, cost control.
