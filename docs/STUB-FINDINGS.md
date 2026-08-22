# Stub Findings Report

Analysis of 8 documented incomplete implementations ("stubs") tracked since
commit `757626e`. Assessed during the `feat/test-quality-gates` branch.
**Reassessed 2026-08-03** against the `dev` source tree; each section below
carries its current resolved/open status, and open items are tracked in
[TODO.md](TODO.md).

## Overall summary

| # | Stub | Effort | Impact | Priority |
|---|------|--------|--------|----------|
| 6 | FTS sync score hardcoded to 1.0 (not 0.0) | ~20 lines | Hybrid search quality | Open — TODO entry |
| 8 | Eval runner integration test ignored | 1 line | CI coverage gap | Open — TODO entry |
| 7 | Tool log overlay detail fields | ~5 lines | Desktop UX | Resolved 2026-08-03 |
| 5 | Global memory namespace | Design + ~50 lines | Feature missing | Resolved 2026-08-03 |
| 3 | WASM plugin completion host fn | Major plumbing | Plugin ecosystem | Resolved 2026-08-03 |
| 1 | Validator ignores prompt_sections | Design + ~30 lines | Config correctness | Resolved 2026-08-01 |
| 4 | Diff state never updated | New state field + wiring | Desktop UX | Resolved 2026-08-03 |
| 2 | Architect normalization dead code | Needs verification | Schema tolerance | Resolved 2026-08-01 (moot) |

---

## Stub 6: FTS sync score hardcoded to 1.0

**Files**: `crates/memory/src/sync.rs:59`, `:90`
**Severity**: Real gap — FTS chunks always enter the store with `score: 1.0`
(not `0.0` as originally reported), and query-time BM25 rank is never read.

### What happens

The `replace_project()` and `insert()` methods of the index sync (`SyncIndex`)
construct `MemoryChunk` values with `score: 1.0` (comment: "neutral default for
stored chunks; query-time FTS rank or vector similarity overwrites this")
before inserting them into the FTS store (`SqliteFullTextStore`). Because
`SqliteFullTextStore::search()` never selects SQLite FTS5 `rank()`, every FTS
chunk is blended with the neutral stored score rather than a real BM25 value.
RRF fusion in `rag.rs` ranks by position, so this is neutral weighting rather
than weightless FTS — but the hybrid ranking never uses the real BM25 signal.

### What should happen

SQLite FTS5 computes BM25 relevance scores natively. The
`SqliteFullTextStore::search()` query needs to include `rank` in its result set,
and the `FtsResult` needs the real BM25 value from SQLite instead of the
neutral default.

### Fix

1. In `fts.rs`, update the `search()` SQL to select `rank` alongside the usual
   columns (the FTS5 `rank` function returns negative values for relevance —
   closer to 0 means more relevant).
2. Populate `FtsResult.score` with `-rank` (to invert to positive).
3. The sync layer no longer needs to populate `score` for FTS chunks — the
   store computes it at query time.

### Risk

Low. The FTS query is a single SQL change. The `MemoryChunk.score` field would
still be `1.0` at insert time but the search result would carry the real score.
This doesn't affect vector embeddings (which have their own score path).

### Resolution (2026-08-03)

Still open. The stored default is `1.0` (`crates/memory/src/sync.rs:59,90`),
not `0.0`, and the real BM25 value remains unwired — the fix above is
untouched. Tracked as the TODO entry "FTS BM25 ranking not wired"
(`docs/TODO.md`).

---

## Stub 8: Eval runner integration test ignored

**File**: `crates/eval/src/runner.rs:414` (crate `concerto-eval`)
**Severity**: Low — test exists but is `#[ignore]`d.

### What happens

`fallback_mode_runs_tests()` is an async integration test that compiles a Rust
project from `eval/benchmark_tasks/standard/`. It was ignored due to slowness.

### What should happen

The test already guards against missing benchmark data (line 418-420: returns
early if `suite_dir` doesn't exist). On CI the path would be absent, so the
test would be a no-op. Locally with the data present it would exercise the
eval runner end-to-end.

### Fix

Remove `#[ignore]`. The test is already safe against missing fixtures.

### Risk

Zero. The early-return guard handles the no-data case.

### Resolution (2026-08-03)

Still open. The test remains `#[ignore]`d at `crates/eval/src/runner.rs:414` in
`concerto-eval` (the runner pipeline is in `concerto-eval`, not
`concerto-eval-runner`). Tracked as the TODO entry "Un-ignore eval end-to-end
test" (`docs/TODO.md`).

---

## Stub 7: Tool log overlay detail fields

**File**: `crates/desktop/src/views/tool_log.rs:113-114`
**Severity**: Low — cosmetic but misleading.

### What happens

When a tool execution starts, the `ToolExecutionStarted` event carries both a
`detail` field (human-readable description of the operation, e.g. "Reading
/etc/config.toml") and an `input_hash` field (content-addressed hash of the
full input). The tool log's `load_stored_events()` maps these to
`ToolLogUpdate::Started { input_summary: detail.unwrap_or_default(), full_input: input_hash }`.

When the user expands a tool log row (lines 297-314), they see:
```
Input: <hash>
Output: <accumulated output>
```

The "Input:" line shows a hash — content-addressed but illegible. The actual
detail content lives in `row.input_summary` but is never displayed in the
expanded view.

### What should happen

Either:
- Show `input_summary` instead of `full_input` in the expanded view, or
- Show both: `input_summary` as a readable label, `full_input` as a
  copyable hash.

### Fix

In the expanded view (line 299), replace `row.full_input` with
`row.input_summary`:

```rust
details.push(text(format!("Input: {}", row.input_summary)).size(12).into());
```

(5-character change, 1 line.)

### Risk

None. `input_summary` is already populated from the event's `detail` field.

### Resolution (2026-08-03)

Resolved. The expanded view now renders `Input: {input_summary}`
(`crates/desktop/src/views/tool_log.rs:316`); the event mapping lives in
`crates/desktop/src/runtime.rs:493-495`. Note the original file reference
`crates/tools/src/tool_log.rs` no longer exists — the tool-log view and its
state live in the desktop crate.

---

## Stub 5: Global memory namespace hard-fails

**File**: `crates/memory/src/system.rs:268-270`
**Severity**: Medium — any code using `MemoryNamespace::Global` crashes.

### What happens

The `MemoryNamespace::Global` variant exists in the type system
(`core/src/memory.rs:48`) and is constructable via
`helpers.rs:global_namespace()`, but the `SystemMemoryStore` refuses to handle
it:

```rust
MemoryNamespace::Global { .. } => Err(CoreMemoryError::RetrievalFailed(
    "global memory is not implemented by the project-scoped memory store".into(),
)),
```

Any code path that tries to store or retrieve global memory (cross-project
preferences, user-level facts, cached personal data) gets a hard error.

### What should happen

Global memory needs a separate SQLite table (in the user's config or data
directory, not the project directory) that stores key-value pairs scoped by
`user_id_hash`. The `SystemMemoryStore` needs either a second pool connection
for global data or a routing layer that dispatches namespace queries to the
right store.

### Design question

- Where does the global DB live? `$XDG_DATA_HOME/concerto/global_memory.db`?
- How does the pool get created and passed through? Same lifecycle as the
  project pool?
- Does global memory use the same schema (chunks with embeddings) or a simpler
  KV model?

### Risk

Medium. Adding a second SQLite pool is straightforward. The design choice
about schema and lifecycle is the blocker, not the implementation.

### Resolution (2026-08-03)

Resolved. `GlobalMemoryStore` (`crates/memory/src/global.rs`) owns a separate
SQLite table for cross-project global state, backed by its own pool at
`<data_dir>/memory/global_memory.db`; the runtime wires it in
`crates/orchestrator/src/runtime_runner.rs:964-977`. `MemoryNamespace::Global`
is handled instead of hard-failing.

---

## Stub 3: WASM plugin completion host function stub

**File**: `crates/plugins/src/host_fns.rs:435-446`
**Severity**: High — plugins that need LLM access cannot use it.

### What happens

The `host_completion()` function is registered as a WASM import (line 458) but
always returns `RESULT_ERROR` with:

```
completion_request requires LLM provider integration (not yet wired)
```

It accepts `_req_ptr` / `_req_len` (a serialized completion request from the
guest) but does nothing with them.

### What should happen

1. Deserialize the `CompletionRequest` from guest memory
2. Route it through a provider factory to an actual LLM provider
3. Run the completion and write the response back to the scratch buffer
4. Return success/error code

### What's missing

The `PluginStoreData` (the per-plugin state stored in WASM `Caller` context)
doesn't hold any reference to a provider factory or LLM provider. Adding one
requires:

- A new field in `PluginStoreData` (e.g. `provider: Option<Arc<dyn LlmProvider>>`)
or a callback function pointer
- Threading the provider through every `PluginStoreData` construction site
  (manager.rs, active_plugin.rs, test setups)
- Error handling when the plugin asks for completion but no provider is
  configured

### Risk

Medium-high. The change touches the core plugin ABI layer. Provider lifecycle
in the plugin host needs careful handling (the provider must be `Send + Sync`,
the WASM guest may call completion from any context).

### Resolution (2026-08-03)

Resolved. `host_completion()` deserializes the guest request and routes it
through the configured LLM provider via `CompletionRequest`
(`crates/plugins/src/host_fns.rs:506-548`), with async host-function support
(ADR-38, wasmtime async_support). Plugins can now request LLM completion.

---

## Stub 1: Validator ignores prompt_sections

**File**: `crates/orchestrator/src/agents/validator.rs:18-19`
**Severity**: Medium — configured instructions for the validator are silently
discarded.

### What happens

The `ValidatorAgent` struct stores `prompt_sections: PromptSections` with
`#[allow(dead_code)]`. The constructor accepts and stores it, but `run()`
never reads it — it delegates directly to `EvalEngine::run()` and produces a
fixed-format summary.

The configured values (from `orchestration_studio.rs`) include:

```rust
system_instructions: "You are the Validator. Run the eval engine...",
constraints: "Never mark a task passing if the build fails...",
output_format: "Pass/Fail, then raw eval output...",
```

All three are ignored.

### What should happen

Unlike Coder/Researcher/Architect/Reviewer (which use `prompt_sections` to
build an LLM prompt), the Validator doesn't call an LLM — it calls
`EvalEngine`. So `prompt_sections` needs a different interpretation:

- **`system_instructions`**: Could be logged or published as an agent thought
  for debugging visibility.
- **`constraints`**: Need to become validation rules applied to `EvalResult`.
  E.g., "fail if tests are skipped/ignored" means post-processing the eval
  output.
- **`output_format`**: Should change how the summary string is formatted.

### What's needed

The `EvalEngine` itself needs configurable validation rules before constraints
have meaning. Either:

1. Extend `EvalEngine` with a `ValidationConfig` parsed from constraints, or
2. Have `ValidatorAgent` post-process `EvalResult` based on constraint rules.

### Risk

Low if option 2 (post-processing in ValidatorAgent). The `EvalEngine` doesn't
need changes, just the validator's `run()` method.

### Resolution (2026-08-01)

The dedicated `ValidatorAgent` is retired (ADR-35 phase 4 / 5, audit A-01).
The validator is now a config seed (`builtin_agent_seeds()` in
`concerto-config`) backed by `GenericSpecialistAgent` in eval-runner mode:
`EvalEngine` runs with no LLM call, and the configured sections are honored
as follows:

- `constraints` → `GenericSpecialistAgent::apply_constraints` (the "fail if
  tests are skipped/ignored" and "never mark a task passing if the build
  fails" rules, ported verbatim from the retired struct);
- `output_format` → `GenericSpecialistAgent::format_summary` (Pass/Fail
  summary prefix);
- `system_instructions` → published as a debug `AgentThought` for visibility.

The residual gap matches the stub's original option 2: `system_instructions`
still does not change the validation logic (the validator never calls an LLM),
it only surfaces for debugging.

---

## Stub 4: Diff state never updated

**Severity**: High — the desktop diff viewer has no data to display.

### What happens

The diff computation layer (`crates/tools/src/diff.rs`) is fully implemented:
- `compute_diff()` produces structured hunks
- `compute_all_virtual_diffs()` aggregates per-file changes from `VirtualFs`
- Tests exist for all diff operations

However, the orchestrator state model (`orchestrator/src/state.rs`) has no
`diff_state` or similar field to accumulate diffs across agent steps. The
search for "diff" in state.rs returned only test-related hits (function names
in test assertions).

### What should happen

The agent loop needs to:

1. After each file write (captured in `VirtualFs`), call
   `compute_all_virtual_diffs()` to get the current diff
2. Store the diff in a new `diff_state` field in `OrchestratorState` or a
   dedicated diff accumulator
3. Wire the accumulated diffs to the desktop UI's diff viewer

### What's needed

- New `DiffState` struct (or use `Vec<DiffResult>` directly)
- A field in `OrchestratorState` (or `AgentRunResult`)
- Post-write diff collection in the agent loop (or in `ToolExecutor`)
- UI wiring to pass accumulated diffs to `App::diff_results`

### Risk

Medium. The diff computation itself works. The gap is plumbing through three
layers: tools → orchestrator state → desktop UI.

### Resolution (2026-08-03)

Resolved. The desktop app computes diffs directly from `VirtualFs` via
`compute_diffs_from_virtual_fs()` (`crates/desktop/src/app.rs:1450`) and feeds
the results into the diff viewer (`app.rs:1452-1483`); no separate
orchestrator `diff_state` field was needed.

---

## Stub 2: Architect normalization dead code

**Files**: `crates/orchestrator/src/agents/architect.rs:162-199`
**Severity**: Depends on whether the code is actually dead or just conservatively
marked.

### What the issue claims

Non-canonical JSON field names from the LLM (like `"proposedFiles"` instead of
`"proposed_files"`, or `"interface"` instead of `"interface_sketch"`) cause
hard schema validation failures because the normalization functions
(`move_alias`, `normalize_string_list`) are dead code.

### Current state

- `move_alias(canonical, aliases)` — maps aliased fields to canonical names
  (line 162-177)
- `normalize_string_list(object, field)` — wraps a plain string or
  non-array value into a JSON array for schema compliance (line 179-198)
- Both accept `#[allow(dead_code)]` annotations

Both are called from `parse_design_doc()` (lines 132-136). The question is
whether `parse_design_doc()` itself is dead code. If the architect's `run()`
method calls `parse_design_doc()`, then the normalization is live and the
issue is stale. If `run()` doesn't call it (e.g. if it was refactored to use
a different parsing path), then the issue is real.

### Recommended action

Verify whether `parse_design_doc()` is called from the architect's `run()`
method. If yes, remove `#[allow(dead_code)]` and close the issue. If no,
re-wire the call or remove the dead functions.

### Resolution (2026-08-01)

Moot: the dedicated `ArchitectAgent` (and `architect.rs`) is retired (ADR-35
phase 4 / 5, audit A-01/H-01). The alias/normalization problem moved into the
typed contract — `SubmitDesignDocInput` in `concerto-core` accepts the legacy
`files` and `interface` field names via serde aliases, and
`GenericSpecialistAgent` validates the submission field-by-field with
structured `ToolResult` feedback (audit H-01).

---

## Recommendations

### Status as of 2026-08-03

Seven of the eight stubs are resolved (1, 2, 3, 4, 5, 7) or moot (2). The two
remaining open items (#6 FTS BM25, #8 eval test un-ignore) are tracked in
[TODO.md](TODO.md); #7 was fixed as originally proposed.

| Stub | Change | Status |
|------|--------|--------|
| #6 | Add BM25 score to FTS query results | Open — tracked in `docs/TODO.md` ("FTS BM25 ranking not wired") |
| #8 | Remove `#[ignore]` from eval test | Open — tracked in `docs/TODO.md` ("Un-ignore eval end-to-end test") |
| #7 | Show `input_summary` instead of `full_input` in tool log | Resolved 2026-08-03 |

### Defer

| Stub | Why |
|------|-----|
| #5 (Global memory) | **Resolved 2026-08-03** — `GlobalMemoryStore` in `crates/memory/src/global.rs`, wired in `runtime_runner.rs`. |
| #3 (Plugin completion) | **Resolved 2026-08-03** — `host_completion()` routes through the configured LLM provider (ADR-38 async host fns). |
| #1 (Validator constraints) | **Resolved 2026-08-01** — option 2 landed via `GenericSpecialistAgent` eval mode (`apply_constraints`/`format_summary`); see Stub 1 resolution note. |
| #4 (Diff state) | **Resolved 2026-08-03** — the desktop computes diffs from `VirtualFs` (`crates/desktop/src/app.rs:1450`). |
| #2 (Architect normalization) | **Resolved 2026-08-01** — moot after the `ArchitectAgent` retirement; serde aliases on `SubmitDesignDocInput` cover the legacy field names; see Stub 2 resolution note. |
