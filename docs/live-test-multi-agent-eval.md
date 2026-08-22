# Concerto Live Test Form — Multi-Agent Eval (strigil run)

**Purpose**: Live verification of a specific change under real conditions.
Use one copy per feature/change, build type, OS, and provider/model
combination. Mark each result **Pass**, **Fail**, **Blocked**, or
**Not tested**. Attach sanitized logs and screenshots for failures. Never
include credentials or private source content.

## Test Environment

| Field | Value |
|---|---|
| Feature/Change Under Test | Multi-agent coordinator (Architect, Researcher, Coder, Reviewer, Validator — write-gated specialists) + eval-engine validation executing a real end-to-end task: build a minimal dependency-free grep clone ("strigil") from a 6-gate prompt. Also exercised the audit-completeness / approval-decision / default-WARN-logging changes (commits `637eea7`, `32cd325`, `dc14b51`, `ae47db7`, `3c75de5`). |
| Branch / PR | `dev` (Concerto). Runtime binary corresponds to `dev` @ **~3c75de5** (evidence: audit records `ApprovedAllForSession` verdicts and per-call approvals → `637eea7`+`3c75de5` in binary; WARN-default `concerto.log` → `dc14b51` in binary; validator false-fail present → `7f464ef` was NOT yet in the binary). |
| Date/Time & Timezone | 2026-08-09 14:47:47 – 15:36:04, UTC+02:00 (`sessions.created_at`, `audit_log`, transcript) |
| Tester | NefaroXX |
| Concerto Commit/Tag | `dev` @ ~3c75de5 (post-run fix `7f464ef` for validator false-fail — see Test Outcome) |
| Build Type | Debug |
| OS/Version | Windows 11 Pro 22000.2538 (tester-provided); artifact copy re-verified on WSL/Linux for checks |
| Frontend | Desktop (Iced) |
| Provider & Model | Provider `prov_01KZH8Z3HJXP45FBVMSBT88ME5`; main agents model "big-pickle"; Validator model_override "deepseek-v4-flash-free" (`config.toml`, `sessions.provider/model`) |
| Shell Profile | `os-git-bash` — 72/72 shell audit rows; zero `cmd.exe` invocations |
| Policy Preset | Default presets + custom specialist capabilities (Validator: `fs_read`, `shell`, `eval`; `fs_write=false`, `git=false`; Reviewer: `git`, `lsp`, `eval`) |
| Memory Enabled (TTL) | Enabled (`memory.enabled = true`, `ttl_days = 30`); project-scoped `SqliteVectorStore` (`memory.db`, `project_id`-keyed) |
| Configuration Highlights | `os-git-bash` shell profile; multi-agent with 5 custom specialists; skills disabled; shell/git approval required (`timeout_seconds = 30`); filesystem largely policy-Allow; default log level WARN |

## Key Tests

| Check | Expected Result | Result/Notes |
|---|---|---|
| Primary flow | Coordinator plans + delegates; Coder produces the artifact; Validator runs eval engine (build/tests) and reports Pass; task completes with `completed=true` | **Pass with defect note** — artifact complete: `Cargo.toml`, `src/main.rs`, `verify.sh`; `verify.sh` exit 0 (15:35:27, audit row); 7 unit tests green (transcript: `test result: ok. 7 passed; 0 failed`). **Defect:** final status reported `completed=false` — Validator false-failed the green suite (see Observations/Outcome; fixed in post-run `7f464ef`). |
| Edge cases & error handling | Malformed tool calls, timeouts, partial failures: no crash; clear error; recovery path | **Pass** — (1) malformed filesystem write ("missing 'content' field") at 15:13:48 → `ExecutionError` recorded in audit, agent recovered; (2) git approval timeout 15:05:47 → denied by default after 30s (WARN 15:06:17) → agent retried → user granted session-wide (`ApprovedAllForSession` 15:06:42), execution proceeded; (3) coordinator plan-JSON parse failure 14:51:49 → heuristic pipeline fallback, run survived (robustness gap noted, not fixed). |
| Configuration & persistence | Per-project config layer reloads on switch; profile selection honored; sessions bound per project | **Pass** — `os-git-bash` used for all 72 shell calls; sessions bound via `project_dir` (`sessions.db`); no cross-project bleed observed (audit rows all carry the strigil project path). |
| Policy/security gating | Approvals/denials work and are fully audited (decision + reason + result) | **Pass** — 43 explicit approvals all recorded with `user_response` (24 shell, 11 git, 4 filesystem, 3 GetDiagnostics, 1 git session-wide); 1 timeout denial (unanswered → denied by default); filesystem mostly policy-Allow (32) with 4 approvals. No bypasses observed. |
| Tool execution & logging | Post-execution audit rows for every tool; failures recorded; app log useful at default level | **Pass** — 194 audit rows; execution rows now present for ALL tools (filesystem 36, shell 24, git 12, GetDiagnostics 3 — previously shell-only); failed write recorded; `concerto.log` contains 8 WARN lines (6 skill-path, 1 coordinator plan-parse, 1 approval-timeout) — the empty-log defect is gone. |
| Cancellation & recovery | Clean stop; partial progress preserved | **Not tested** — no cancellation exercised in this run. |
| Spend & audit tracking | Per-run spend and per-tool metrics recorded | **Pass** — session totals 258,355 tokens in / 23,094 out, $0.8768 (sessions table); coordinator reported $1.2826 for the multi-agent task; per-tool metrics in audit/providers tables. |
| UX & feedback | Status indicators, approval dialogs, errors, completion banner | **Inspection by tester** — observed behaviors from data: 43 approval dialogs surfaced (per-call prompting), GrantAlways granted session-wide approvals once; final UI banner reported "Validation still fails after 2 automatic recovery cycles" (now understood: validator false-fail, fixed post-run). |

## Automated Checks

Artifact checks run against the delivered project (`/mnt/Temp/strigil`, debug build; WSL/Linux copy for this review):

```bash
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo build
cargo test          # 7 unit tests; nextest also available on Windows (tester)
./verify.sh         # run's own acceptance script (5 checks)
```

| Check | Result | Notes |
|---|---|---|
| cargo fmt --check | **Pass** | rustfmt default (no project rustfmt.toml). |
| Clippy (-D warnings) | **Pass** | clean, 0 warnings. |
| Build (debug) | **Pass** | `Finished dev profile`, 2.8s. |
| Tests | **Pass** | `test result: ok. 7 passed; 0 failed; 0 ignored`. |
| Nextest | **Not run here** | `cargo-nextest` installed in the Windows environment (not visible in the WSL shell used for checks) — run `cargo nextest run` on Windows if desired. |
| Cargo Deny | **N/A** | Artifact ships no `deny.toml`; zero external dependencies (stdlib only; Cargo.lock = 1 package) — nothing to audit. |
| verify.sh (acceptance) | **Pass** | All 5 checks passed (basic match, no-match exit 1, `--ignore-case`, wrong args exit 2, `COLOR=always` ANSI). |

Concerto-side checks run during this session (not part of this form's artifact): `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p concerto-orchestrator` (343 lib + 4 parity + 1 doc tests, incl. new `apply_constraints` regression tests), `cargo check --workspace` — all green.

## Test Outcome

- Complex Task: Multi-agent coordinated build of a small publishable CLI tool from a 6-gate prompt, under per-call policy approvals, with eval-engine validation.
- Observations:
  - **Validator false-fail (blocking defect, root-caused and fixed post-run, `7f464ef`):** the seeded Validator constraints read "Never mark a task passing if the build fails or tests are skipped/ignored". `apply_constraints` armed the skip/ignore rule on the bare substring `"ignore"`, then matched the literal word `"ignored"` in output — libtest always prints `0 ignored` on a green run, so every green suite was forced to `Fail` (this run: 2 validation cycles → escalation → `completed=false`). Fix: phrase-triggered arming (`skipped`/`ignored` words) + count-aware output detection (`N ignored` with N>0 fails; `0 ignored` passes); regression tests mirror the live failure.
  - Coordinator plan-JSON parse failure once (14:51:49) → heuristic pipeline fallback; run survived. Robustness gap tracked for follow-up.
  - One malformed filesystem write (missing `content` field) — tool errored cleanly, error audited, agent recovered.
  - One approval timeout (git) — denied by default, agent retried, user granted session-wide; audit recorded the full chain.
  - All audit/logging/approval fixes from this session verified working under live conditions (see Key Tests).
- Expected vs Actual: Expected: task completes as `completed=true` with a Pass validation. Actual: artifact fully built and green (`verify.sh` 5/5), but validation reported `Fail` twice and the run ended `completed=false` — caused entirely by the validator false-fail heuristic, which is fixed in post-run commit `7f464ef`. A re-run of the same task should now report Pass end-to-end.
- Build/Test Result: **Pass** — artifact builds cleanly, 7/7 unit tests, `verify.sh` 5/5, fmt + clippy clean.
- Final Status & Defects: Task **partially passed with defect found & fixed**: blocking defect `7f464ef` (validator false-fail on green suites — affects any run using the default Validator preset); open non-blocking item: coordinator plan-parse fallback (no retry). Artifact itself: **Pass** with one spec deviation (IO errors exit 1, prompt Gate 5/6 demanded 3 — prompt internally contradictory; `verify.sh` does not test the required "missing file → exit 3" case).

**Funding Notes**: 258,355 in-tokens / ~$1.28 total for a complete, tested, dependency-free CLI artifact. Efficient: one coordinator plan (with one fallback), two validation cycles (both consumed by the false-fail bug), no wasted shell runs (all `os-git-bash`, no hangs). Cost control acceptable; the false-fail bug wasted ~2 validation cycles and one escalation of budget.

## Round 3 — stored-plan approval + Apply execution (M3 loop verification)

Follow-up round on the ADR-55 plan-approval path. Data dir `/mnt/Temp/concerto`;
runtime `dev` @ **bff3e85** (backed by strigil-run fixes); verdict workspace at
`C:\Users\Sol\Desktop\Projects\verdict`.

- **Tested**: bare "i approve" approval-of-stored-plan exercise — close the
  plan loop with only the approval phrase.
- **PASS — routing/approval loop (verified end-to-end)**: `plan:` → Plan route →
  rendered plan → artifact `01KZPZKVQAEQRNK9Q7WVVJRJE2` bound → bare "i approve"
  armed the Apply dialog (audit `intent:plan | apply | {"plan_id":"01KZPZKVQAEQRNK9Q7WVVJRJE2",...}`) →
  binding consumed (`plan_bindings` count 0 after Apply) → Execute run.
- **FAIL — execution half**: coordinator subtasks were literally
  `Implement: i approve <conversation_history>…`; Coder produced zero files;
  Reviewer flagged Critical (zero workspace entries); validation config absent,
  so validation could not run. `concerto.log` twice:
  `Task planning failed: MultiAgentPlanFailed { reason: "JSON parse error: expected a JSON array of plan items, got: " }`
  — empty planner response (deepseek-v4-flash-free) → heuristic fallback built
  degenerate subtasks from the task text. Root causes: run task built from
  `req.input` (the approval phrase); empty planner output fell through to the
  JSON parser.
- **Fix record**: commit `b228837` — Apply builds the run task from the stored
  approved plan (capture-before-consume via `approved_plan_task_description` /
  `build_run_task`); planner empty/whitespace responses retry once, then fail
  with an explicit `MultiAgentPlanFailed` reason. Workspace gate green (2511
  tests, 25 crates).
- **Expected next live test**: re-run the same exercise on the new binary —
  expect the Coder to execute the stored plan and produce files, then normal
  review → validation; watch `concerto.log` for the planner empty-response
  retry (warn + one retry, then the explicit failure reason).

## Round 4 — binding-driven Apply arming

Follow-up round on the ADR-55 plan-approval path (§11, live-fix round 4). Data
dir `/mnt/Temp/concerto`; runtime `dev` @ **ba41f2d** (the §11 fix commit).

- **Tested**: `plan:` (spec) → rendered plan bound under
  `01KZQ4VAB3VDEMRSACFYK0W5TS` → user typed "execute" — a follow-up utterance
  that is a confident Execute with no phrase/hash binding.
- **PASS — routing, durability, diagnostics**: `plan:` routed Plan
  (`negation_override`); the "execute" follow-up routed Execute (`ask_user`,
  granted); the planner retry warning fired live (`planner returned an empty
  response; retrying once provider=opencode attempt=1`); the git hint was
  live; the durable `plan_bindings` row stayed intact (count 1, plan
  `01KZQ4VAB3VDEMRSACFYK0W5TS`).
- **FAIL — no dialog armed**: arming was phrase- and hash-only, so the
  confident Execute surfaced no Apply/Replan dialog (no `intent:plan | apply`
  audit row). The coordinator re-planned from the raw "execute"; the planner
  returned empty (explicit `MultiAgentPlanFailed`, twice); the heuristic
  fallback built degenerate subtasks (`Implement: execute …`); the Coder made
  zero write tool calls (audit: observe/read/probe rows only); the Reviewer
  flagged Critical twice ("workspace root contains no files at all"); two
  `provider stream-idle timed out after 120s`; the run ended "Task failed:
  provider stream-idle timed out after 120s".
- **Fix record**: ADR-55 §11 — third arming fallback in `run_shared_agent`
  (`bound.is_none() && is_confident_execute(&routing)` →
  `store.load_newest_plan_binding` → `arm_binding_for_confident_execute`),
  commit `ba41f2d`
  (`fix(orchestrator): arm Apply/Replan dialog for confident Execute from
  durable session binding`). Workspace gate green (2514 tests, 25 crates).
- **Expected next live test**: re-run `plan:` → execute on the new binary —
  the Apply dialog should now arm with the stored plan; Apply should run the
  approved plan text; then the agent-specific issues (Coder writes, provider
  stream-idle timeouts) are the next triage target.

## Round 5 — bare execution directives arm the stored-plan dialog (§12)

Follow-up round on the ADR-55 plan-approval path (§12, live-fix round 5). Data
dir `/mnt/Temp/concerto`; date 2026-08-11; session
`01KZRHY1J5EVMX4D240K60M7MT`; runtime `dev` @ **ba41f2d** (the §11 binary —
the §12 fix was not yet in the tested binary).

- **Tested**: `plan:` (spec) → rendered plan bound under
  `01KZRJ0XRRPYTRFQ2SZ9ZYD74E` (objective hash `132befb4471cfa15`, never
  consumed) → user typed bare "execute" and bare "approve" as follow-ups.
- **FAIL — symptom**: both bare follow-ups surfaced the generic AskUser list
  modal ("I could not confidently tell what you want. Pick the intent for this
  run") instead of the stored-plan Apply/Replan dialog, even though the
  session's newest durable binding existed and was unarmed.
- **Evidence**: audit rows `intent_router | granted | ask_user | Execute` for
  both inputs (`rule_matched = ask_user`); the durable `plan_bindings` row was
  present but unarmed — no `intent:plan | apply` audit row; the §11 fallback
  never fired.
- **Root cause**: `EXECUTE_KEYWORDS` lacked the base forms
  "execute"/"run"/"apply"/"approve", so a bare directive never hit the
  deterministic Execute rule and classification fell to the AskUser path;
  the `is_plan_approval_phrase` list also lacked the bare/run-family phrasings
  ("run the plan", "run plan").
- **Fix record**: ADR-55 §12 — `EXECUTE_KEYWORDS` base forms (word-boundary
  only, no inflections), `VERIFY_KEYWORDS` run-family phrasings ("run tests",
  "run the test", "run the test suite", "run cargo test", "run the build"),
  and `is_plan_approval_phrase` += "run the plan" / "run plan". Priority order
  unchanged (negation → question → Verify → Plan → Review → Diagnose →
  Execute); no new grant surface — arming still lands in the user-facing
  confirm dialog. Workspace gate green (2514 tests), clippy + fmt clean. Landing commit `4778ea3`.
- **Secondary observations (non-blocking, deferred)**: planner empty-response
  retries still occur with the opencode provider, and a
  `failed to persist multi-agent failure store_error=database error:
  operation cancelled` appeared when the run was interrupted.
- **Expected next live test**: re-run `plan:` → bare "execute" / "approve" on
  the §12 binary — the Apply dialog should arm with the stored plan; Apply
  should run the approved plan text.

## Round 6 — intent gate verified end-to-end in the live app (post-§12)

Follow-up round on the ADR-55 plan-approval path (post-§12, live-fix round 6).
Data dir `/mnt/Temp/concerto`; date 2026-08-11; session
`01KZRQNWASZXSDCSYGCTKZREP4`; project verdict on a Windows host; provider
opencode/deepseek-v4-flash-free; total spend $1.77.

- **Tested**: `plan:` (spec) → rendered plan → approval phrase "i approve".
  Messages: 1 = user "plan: Build a minimal, dependency-free CLI tool named
  `verdict`…"; 2 = assistant rendered "# Plan" (16 KB); 3 = user "i approve".
- **PASS — routing/approval loop (verified end-to-end)**: plan run → router
  `negation_override → Plan`; planner returned empty twice
  (15:41:18/15:41:32) → `MultiAgentPlanFailed` → heuristic fallback → plan
  artifact `01KZRQRW2P9XXPS9JW0R42EYJG` bound to the session. At 15:47:39 the
  user's "i approve" armed the stored-plan Apply/Replan dialog (NOT the generic
  AskUser list modal): audit row
  `intent:plan | apply | {"plan_id":"01KZRQRW2P9XXPS9JW0R42EYJG","source_revision":null}`;
  router classified `execute_keyword → Execute` (the ADR-55 §12 vocabulary
  addition is live: "approve" now routes Execute/0.8; the approval-phrase path
  armed the dialog first). Apply consumed the durable binding: `plan_bindings`
  empty afterward (no leak, restart-safe).
- **PASS — execution half (round-3 `b228837` behavior confirmed live)**: the
  execute run's task was built from the stored plan text: artifact
  `plan-01KZRR67XCF673ABRQTX9WQX5M.json` (94 KB) has `task_description`
  "Execute the approved plan (plan 01KZRQRW2P9XXPS9JW0R42EYJG) for this
  objective: # Plan …". The execute run did real work: tool version checks,
  `.scratch` diff-fixture creation and verification via approved shell
  commands, GetDiagnostics/GetSemanticTokens, and the reviewer issued a
  Critical revision (missing Cargo scaffold) → coder queued the revise
  subtask.
- **Run outcome — cancellation (not an intent-gate defect)**: run ended in
  cancellation at 16:00:15: `ProviderRetryExhausted` on the coder's revision
  subtask → `SubTaskCancelled` → `failed to persist multi-agent failure
  store_error=database error: operation cancelled`. Same deferred
  agent/provider class as before: planner emptied again at 15:48 during the
  execute run's coordinator (handled by heuristic fallback), provider retry
  exhaustion, interrupted-run persist failure.
- **Conclusion**: ADR-55 intent gate verified working end-to-end in the live
  app — the rounds 4/5 failure mode is eliminated. The §12 bare-word path
  ("execute"/"approve" alone) remains unit-pinned but not yet live-exercised —
  the user typed "i approve" (phrase path).
- **Related commits**: `4778ea3` (§12 fix), `a318dab` (ADR-55 §12 + T18 +
  Round 5 docs).