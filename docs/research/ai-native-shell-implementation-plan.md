# Implementation Plan: AI-Native Shell for Concerto
## Closed-World, Schema-Rigid, Cross-Platform Tool Runtime

**Project:** Concerto  
**Target Crate:** `concerto-shell`  
**Date:** 2026-08-01  
**Status:** Scoped for immediate development

---

## Phase 0: Foundation (Weeks 1–2)

### 0.1 Define the `ToolManifest` Schema System

Create a new module in `concerto-shell` that defines the canonical tool description format. This is the contract between the shell and the LLM.

**Deliverables:**
- `ToolManifest` struct with:
  - `name: &'static str` (e.g., `fs.read`, `git.status`)
  - `version: SemVer` (pinned, hashed)
  - `schema: JsonSchema` (input parameters, strict mode)
  - `output_schema: JsonSchema` (structured output contract)
  - `side_effects: EffectClass` (`ReadOnly | Idempotent | Destructive`)
  - `max_output_bytes: usize` (hard limit)
  - `description: &'static str` (semantic constraints embedded here)
- `ToolRegistry` — compile-time registry using `phf` or `linkme` for static registration
- `SchemaValidator` — wrapper around `jsonschema` crate for pre-dispatch validation
- `ToolOutput` enum:
  ```rust
  pub enum ToolOutput {
      Full(JsonValue),              // < 1KB
      Summarized(Summary),          // Rule-based or LLM-generated
      Truncated { summary: Summary, full_artifact: FileRef },
  }
  ```

**Acceptance Criteria:**
- [ ] Every built-in tool has a manifest that compiles into the binary
- [ ] Schema validation rejects calls with `additionalProperties` not in schema
- [ ] `EffectClass` is used by policy engine for gating

---

### 0.2 Establish the `ToolExecutor` Validation Layer

Modify the existing `ToolExecutor` in `concerto-shell` to validate every incoming tool call against the manifest before dispatch.

**Deliverables:**
- Pre-dispatch validation step in `ToolExecutor::execute()`:
  1. Look up tool name in `ToolRegistry`
  2. If not found → return `ToolError::UnknownTool` with available tool list
  3. Validate JSON arguments against `schema` using `SchemaValidator`
  4. If invalid → return `ToolError::InvalidArgument` with field-level hints
  5. Check policy engine rules (existing)
  6. Check `EffectClass` against current policy mode
  7. Dispatch to tool implementation
- `ToolError` enum with structured, corrective feedback:
  ```rust
  pub enum ToolError {
      UnknownTool { name: String, available: Vec<String> },
      InvalidArgument { field: String, expected: String, hint: String },
      SchemaViolation { detail: String },
      PolicyDenied { rule: String },
      OutputLimitExceeded { limit: usize, actual: usize },
  }
  ```

**Acceptance Criteria:**
- [ ] Invalid tool calls are rejected before any side effects occur
- [ ] Error messages include corrective hints (e.g., `"Unknown parameter: include_deactivated. Hint: search_users returns active users only."`)
- [ ] Validation overhead < 1ms per call

---

### 0.3 Port Existing Tools to Manifest-Native Versions

Migrate Concerto's most-used tools from free-form execution to manifest-native, schema-defined tools.

**Priority order:**
1. `fs.read` — read file with structured output
2. `fs.write` — write file with idempotency keys
3. `fs.search` — structured search (replaces `grep`)
4. `fs.list` — directory listing as structured table
5. `process.spawn` — cross-platform process execution
6. `git.status` — git operations via `git2` crate

**Deliverables per tool:**
- Rust implementation in new `tool-*` crate (see Phase 1)
- `ToolManifest` with strict JSON schema
- Structured `ToolOutput` (not raw text)
- `dry_run` support for destructive operations
- Unit tests with schema validation

**Acceptance Criteria:**
- [ ] All 6 priority tools have manifests and strict schemas
- [ ] `fs.read` returns JSON with content + metadata, not raw text
- [ ] `fs.write` supports `expect_hash` idempotency key
- [ ] `fs.search` returns structured results (path, line, match) instead of grep text

---

## Phase 1: Cross-Platform Built-in Tool Crates (Weeks 3–6)

### 1.1 Extract `tool-fs` Crate

Pure Rust filesystem operations with structured output.

**Tools:**
| Tool | Schema | Output | Replaces |
|------|--------|--------|----------|
| `fs.read` | `{ path: string, offset?: int, limit?: int }` | `{ content: string, size: int, hash: string, lines: int }` | `cat` |
| `fs.write` | `{ path: string, content: string, expect_hash?: string, dry_run?: bool }` | `{ bytes_written: int, new_hash: string }` | `echo > file` |
| `fs.search` | `{ pattern: string, path: string, regex?: bool, max_results?: int }` | `[{ path, line, match, context }]` | `grep -r` |
| `fs.list` | `{ path: string, recursive?: bool, filter?: string }` | `[{ name, type, size, modified, permissions }]` | `ls -la` |
| `fs.diff` | `{ path_a: string, path_b: string }` | `{ changes: [{ type, line, content }] }` | `diff` |
| `fs.glob` | `{ pattern: string, cwd?: string }` | `[{ path, type, size }]` | `find` |

**Implementation:**
- Use `std::fs` + `walkdir` + `regex` crates
- Cross-platform path handling via `std::path::PathBuf`
- `memmap2` for efficient large file reading
- Content hashing via `blake3` for idempotency keys

**Acceptance Criteria:**
- [ ] All 6 tools pass on Linux, macOS, Windows
- [ ] `fs.search` handles 10k files in < 100ms
- [ ] `fs.read` with `limit` prevents context bloat
- [ ] `fs.write` with `dry_run` returns preview without writing

---

### 1.2 Extract `tool-process` Crate

Cross-platform process management without relying on `sh` or `bash`.

**Tools:**
| Tool | Schema | Output |
|------|--------|--------|
| `process.spawn` | `{ cmd: string[], cwd?: string, env?: map, timeout?: int }` | `{ pid: int, stdout: string, stderr: string, exit_code: int }` |
| `process.list` | `{}` | `[{ pid, name, cpu, memory, status }]` |
| `process.kill` | `{ pid: int, signal?: string }` | `{ success: bool }` |
| `process.env` | `{ key?: string }` | `map<string, string>` or single value |

**Implementation:**
- Use `tokio::process` for async execution
- `sysinfo` crate for cross-platform process listing
- Structured stdout/stderr capture (not raw streams)
- Timeout handling with graceful termination

**Acceptance Criteria:**
- [ ] `process.spawn` works on all three platforms
- [ ] `process.list` returns structured data, not `ps` text
- [ ] Timeout kills process tree, not just parent

---

### 1.3 Extract `tool-git` Crate

Git operations via the `git2` crate — no `git` binary required.

**Tools:**
| Tool | Schema | Output |
|------|--------|--------|
| `git.status` | `{ path?: string }` | `{ branch: string, ahead: int, behind: int, changes: [{ path, status }] }` |
| `git.log` | `{ max_count?: int, path?: string }` | `[{ hash, author, date, message }]` |
| `git.diff` | `{ staged?: bool, path?: string }` | `{ files: [{ path, additions, deletions, hunks }] }` |
| `git.branch` | `{}` | `[{ name, current, remote }] }` |

**Implementation:**
- Pure Rust via `git2` (libgit2 bindings)
- No shelling out to `git` binary
- Structured diff output with hunk information

**Acceptance Criteria:**
- [ ] Works in repos without `git` installed on the host
- [ ] `git.status` returns structured change list
- [ ] `git.diff` returns parseable hunks, not raw `git diff` text

---

### 1.4 Extract `tool-search` Crate

Fast content search with structured results.

**Tools:**
| Tool | Schema | Output |
|------|--------|--------|
| `search.content` | `{ pattern: string, path: string, regex?: bool, case_sensitive?: bool, max_results?: int }` | `[{ path, line, column, match, context_before, context_after }]` |
| `search.symbols` | `{ path: string, language?: string }` | `[{ name, kind, line, signature }]` |

**Implementation:**
- `regex` crate with `memmap2` for fast file scanning
- Tree-sitter integration for symbol extraction (leverage existing memory crate logic)
- Result streaming with early termination support

**Acceptance Criteria:**
- [ ] Search 100k lines in < 50ms
- [ ] Streaming stops when `max_results` reached
- [ ] Symbol extraction works for Rust, Python, TypeScript

---

### 1.5 Extract `tool-archive` Crate

Zip/tar operations via pure Rust.

**Tools:**
| Tool | Schema | Output |
|------|--------|--------|
| `archive.extract` | `{ archive: string, destination: string, filter?: string }` | `{ files: [{ path, size }] }` |
| `archive.create` | `{ destination: string, source: string[], format: "zip" | "tar" }` | `{ bytes_written: int, files: int }` |

**Implementation:**
- `zip` crate for ZIP files
- `tar` crate for TAR files

---

### 1.6 Extract `tool-net` Crate

HTTP and network operations.

**Tools:**
| Tool | Schema | Output |
|------|--------|--------|
| `net.http` | `{ url: string, method?: string, headers?: map, body?: string, timeout?: int }` | `{ status: int, headers: map, body: string, duration_ms: int }` |
| `net.ping` | `{ host: string, port?: int, timeout?: int }` | `{ reachable: bool, latency_ms: int }` |

**Implementation:**
- `reqwest` for HTTP
- `tokio::net` for TCP checks

---

## Phase 2: Context Optimization (Weeks 7–9)

### 2.1 Implement Output Summarization Tiers

**Deliverables:**
- Configurable summarizers in `ToolOutput`:
  - `RuleBasedSummarizer` — regex/line-count based for common formats
  - `AstSummarizer` — tree-sitter based for code files (signatures, imports, doc comments)
  - `PassThroughSummarizer` — for small outputs (< 1KB)
- `Summary` struct:
  ```rust
  pub struct Summary {
      pub description: String,      // "42-line Rust file with 3 functions"
      pub key_points: Vec<String>,  // ["fn main() -> Result<()>", "fn helper(x: i32)"]
      pub metadata: JsonValue,      // line count, language, imports
      pub full_ref: FileRef,        // SQLite reference to full content
  }
  ```
- Integration with existing `memory` crate: large outputs → SQLite, prompt gets `Summary`

**Acceptance Criteria:**
- [ ] Code file > 100 lines returns AST summary, not full content
- [ ] JSON file > 10KB returns structure summary, not full content
- [ ] Agent can request specific line ranges via `fs.read` with `offset`/`limit`

---

### 2.2 Hierarchical Tool Namespace Loading

**Deliverables:**
- Tool namespaces: `fs.*`, `git.*`, `build.*`, `net.*`, `process.*`, `search.*`
- `ToolWorkspace` struct tracking active namespaces
- Orchestrator integration: load namespaces based on current plan phase
- `tool_choice` optimization: only expose relevant tools to LLM

**Algorithm:**
```rust
pub fn active_tools_for_phase(phase: PlanPhase) -> Vec<&ToolManifest> {
    match phase {
        PlanPhase::Exploration => vec![&FS_READ, &FS_LIST, &SEARCH_CONTENT, &GIT_STATUS],
        PlanPhase::Coding => vec![&FS_READ, &FS_WRITE, &FS_SEARCH, &PROCESS_SPAWN, &GIT_DIFF],
        PlanPhase::Validation => vec![&PROCESS_SPAWN, &FS_READ, &NET_HTTP],
        PlanPhase::Deployment => vec![&PROCESS_SPAWN, &NET_HTTP, &ARCHIVE_CREATE],
    }
}
```

**Acceptance Criteria:**
- [ ] Tool section of prompt < 2k tokens in any phase
- [ ] Namespace transitions are logged and observable
- [ ] Agent can request namespace expansion if needed

---

### 2.3 Idempotency Keys & State Deltas

**Deliverables:**
- `expect_hash` field on all destructive tools (`fs.write`, `process.spawn` with state changes)
- Hash computation via `blake3` on file content / environment state
- Delta return on conflict:
  ```rust
  pub enum WriteResult {
      Success { new_hash: String },
      Conflict { current_hash: String, diff: Vec<DiffHunk> },
  }
  ```
- `process.spawn` idempotency via environment hash + command hash

**Acceptance Criteria:**
- [ ] `fs.write` with wrong `expect_hash` returns current hash + diff
- [ ] Agent can resolve conflict in one round trip (no read-then-write)
- [ ] Hash computation < 1ms for files < 1MB

---

## Phase 3: Deterministic Execution (Weeks 10–12)

### 3.1 Observable Shell State Machine

**Deliverables:**
- `ShellState` struct exposed as queryable tool:
  ```rust
  pub struct ShellState {
      pub cwd: PathBuf,
      pub env: HashMap<String, String>,
      pub last_exit_code: i32,
      pub open_files: Vec<PathBuf>,
      pub running_processes: Vec<ProcessInfo>,
      pub active_namespaces: Vec<String>,
      pub session_id: Uuid,
  }
  ```
- `shell.state` tool — returns current state as JSON
- `shell.history` tool — returns recent tool calls with outcomes
- Integration with existing session persistence

**Acceptance Criteria:**
- [ ] Agent can query state without running `pwd` or `env`
- [ ] State updates after every tool call
- [ ] History available for debugging loops

---

### 3.2 Dry-Run & Confirmation Modes

**Deliverables:**
- `dry_run: bool` parameter on all destructive tools
- Dry-run result format:
  ```rust
  pub struct DryRunResult {
      pub would_succeed: bool,
      pub description: String,  // "Would overwrite 42 bytes at main.rs (current hash: abc123)"
      pub side_effects: Vec<String>,
      pub policy_check: PolicyResult,
  }
  ```
- Policy engine integration: `EffectClass::Destructive` requires confirmation in restricted mode
- Desktop UI integration: show dry-run preview in diff view

**Acceptance Criteria:**
- [ ] `fs.write` with `dry_run: true` never writes to disk
- [ ] Description is actionable for the LLM
- [ ] Policy engine gates destructive operations in restricted mode

---

### 3.3 Streaming Tool Results with Early Termination

**Deliverables:**
- `ToolExecutor` supports streaming via `tokio::sync::mpsc`
- Orchestrator can send `cancel` signal after receiving enough information
- Tools support `max_results` and `max_bytes` limits
- Early termination for: `search.content`, `process.spawn` (long builds/tests), `fs.list` (deep recursion)

**Acceptance Criteria:**
- [ ] Search stops after `max_results` without scanning entire tree
- [ ] Build output streams in real-time, cancellable
- [ ] Cancellation leaves no orphaned processes

---

## Phase 4: Integration & Distribution (Weeks 13–16)

### 4.1 Shell Profile: Restricted Mode

**Deliverables:**
- New shell profile: `restricted` (complement to existing `standard`, `expert`)
- In `restricted` mode:
  - Only Concerto-native tools available
  - `shell.exec` disabled entirely
  - All destructive tools require `dry_run` first or explicit user confirmation
  - Tool namespaces loaded hierarchically
- Policy rules updated to enforce restricted mode
- Desktop UI: profile selector with clear descriptions

**Acceptance Criteria:**
- [ ] `restricted` mode blocks all raw shell execution
- [ ] Agent cannot bypass via tool name variations
- [ ] Profile is persisted per-project

---

### 4.2 Single Binary Distribution

**Deliverables:**
- Static linking configuration for `concerto` binary
- Embedded assets via `include_str!` / `include_bytes!`:
  - Tool manifests (JSON schemas)
  - Default policy rules
  - Shell profile templates
  - Standard library stubs
- Cross-compilation targets:
  - `x86_64-unknown-linux-gnu`
  - `aarch64-unknown-linux-gnu`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
  - `x86_64-pc-windows-msvc`
- CI pipeline for building all targets
- Installer script for each platform

**Acceptance Criteria:**
- [ ] Single binary runs on fresh machine with zero dependencies
- [ ] Binary size < 50MB with all tools embedded
- [ ] Cross-compilation works from Linux host
- [ ] GitHub Actions builds all 5 targets in parallel

---

### 4.3 Provider-Native Function Calling Integration

**Deliverables:**
- `ToolManifest` → OpenAI function schema converter
- `ToolManifest` → Anthropic tool_use schema converter
- `ToolManifest` → Gemini function declaration converter
- Strict mode enforcement: always set `strict: true`, `additionalProperties: false`
- Provider capability detection: fallback to structured text only if function calling unavailable

**Acceptance Criteria:**
- [ ] OpenAI provider uses native function calling with strict mode
- [ ] Anthropic provider uses native tool_use
- [ ] Schema conversion is lossless and tested
- [ ] Fallback mode is clearly marked in logs

---

### 4.4 Multi-Agent Validation Integration

**Deliverables:**
- `Validator` agent role for tool call validation
- `Critic` agent role for post-execution review
- Swarm integration with existing multi-agent coordinator
- Validation hooks:
  - Pre-execution: Validator checks tool call against schema and policy
  - Post-execution: Critic checks output against expected schema
  - On error: Validator suggests corrective action

**Acceptance Criteria:**
- [ ] Validator catches 90%+ of invalid tool calls before execution
- [ ] Critic detects hallucinated success claims
- [ ] Validation adds < 500ms latency per critical operation

---

## Phase 5: Evaluation & Hardening (Weeks 17–20)

### 5.1 Hallucination Benchmark Suite

**Deliverables:**
- `eval-runner` test cases for tool hallucination:
  - Unknown tool calls (should reject)
  - Invalid parameters (should reject with hints)
  - Invented flags (should reject)
  - Wrong types (should reject)
  - Missing required fields (should reject)
- Baseline measurement: run benchmark against current shell
- Target: 99.9% rejection rate for invalid calls

**Acceptance Criteria:**
- [ ] Benchmark covers 50+ hallucination scenarios
- [ ] 99.9% of invalid calls rejected before execution
- [ ] Benchmark runs in CI on every PR

---

### 5.2 Context Efficiency Benchmark Suite

**Deliverables:**
- Measure tokens consumed per task:
  - File exploration task
  - Refactoring task
  - Debug task
- Compare against baseline (current text-based shell)
- Target: 60% reduction in tool-related context tokens

**Acceptance Criteria:**
- [ ] Context reduction measured and logged
- [ ] 60% reduction target met for standard tasks
- [ ] Regression tests prevent context bloat creep

---

### 5.3 Cross-Platform Integration Tests

**Deliverables:**
- GitHub Actions matrix: Linux, macOS, Windows
- Tests for each `tool-*` crate on all platforms
- File path handling tests (Windows paths vs Unix paths)
- Process spawn tests (shell escaping differences)
- Git operations tests (line ending differences)

**Acceptance Criteria:**
- [ ] All tests pass on all 3 platforms
- [ ] No platform-specific test skips
- [ ] Release blocked on CI failure

---

## Dependency Graph

```
Phase 0 (Foundation)
├── 0.1 ToolManifest Schema
├── 0.2 ToolExecutor Validation
└── 0.3 Port Existing Tools
    │
Phase 1 (Built-ins)
├── 1.1 tool-fs
├── 1.2 tool-process
├── 1.3 tool-git
├── 1.4 tool-search
├── 1.5 tool-archive
└── 1.6 tool-net
    │
Phase 2 (Context Opt)
├── 2.1 Output Summarization
├── 2.2 Hierarchical Loading
└── 2.3 Idempotency Keys
    │
Phase 3 (Determinism)
├── 3.1 Shell State Machine
├── 3.2 Dry-Run Modes
└── 3.3 Streaming + Cancel
    │
Phase 4 (Integration)
├── 4.1 Restricted Profile
├── 4.2 Single Binary
├── 4.3 Provider Integration
└── 4.4 Multi-Agent Validation
    │
Phase 5 (Hardening)
├── 5.1 Hallucination Benchmark
├── 5.2 Context Benchmark
└── 5.3 Cross-Platform Tests
```

---

## Crate Structure

```
concerto/
├── crates/
│   ├── shell/                    # Existing: concerto-shell
│   │   ├── src/
│   │   │   ├── manifest.rs       # NEW: ToolManifest, ToolRegistry
│   │   │   ├── validator.rs      # NEW: SchemaValidator
│   │   │   ├── executor.rs       # MODIFIED: pre-dispatch validation
│   │   │   ├── output.rs         # NEW: ToolOutput, Summary
│   │   │   ├── state.rs          # NEW: ShellState
│   │   │   └── namespaces.rs     # NEW: ToolWorkspace, phase-based loading
│   │   └── Cargo.toml
│   ├── tool-fs/                  # NEW
│   ├── tool-process/             # NEW
│   ├── tool-git/                 # NEW
│   ├── tool-search/              # NEW
│   ├── tool-archive/             # NEW
│   ├── tool-net/                 # NEW
│   ├── plugins/                  # Existing: WASM plugin host
│   ├── plugin-sdk/               # Existing: plugin interface
│   ├── orchestrator/             # Existing: modified for namespaces
│   ├── memory/                   # Existing: integration with summaries
│   └── eval-runner/              # Existing: new benchmark suites
```

---

## Resource Requirements

| Phase | Duration | Complexity | New Crates | Modified Crates |
|-------|----------|------------|------------|-----------------|
| 0 | 2 weeks | Medium | 0 | 3 (shell, orchestrator, policy) |
| 1 | 4 weeks | High | 6 | 1 (shell registry) |
| 2 | 3 weeks | Medium | 0 | 4 (shell, memory, orchestrator, providers) |
| 3 | 3 weeks | Medium | 0 | 3 (shell, orchestrator, desktop) |
| 4 | 4 weeks | High | 0 | 5 (shell, config, desktop, CI, providers) |
| 5 | 4 weeks | Medium | 0 | 2 (eval-runner, test harness) |
| **Total** | **20 weeks** | | **6** | **~10** |

---

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `git2` crate limitations vs full git CLI | Medium | Medium | Document unsupported operations; fallback to `tool-process` spawn for edge cases |
| Cross-platform path/permission differences | High | Low | Extensive CI testing; `std::path::PathBuf` abstraction |
| Binary size bloat from embedded assets | Medium | Medium | Compress schemas; lazy-load large assets; measure in CI |
| Provider strict mode schema limitations | Medium | High | Schema normalization layer; test conversion for all providers |
| Agent performance regression from validation | Low | Medium | Benchmark validation overhead; optimize hot paths |
| User resistance to restricted mode | Medium | Low | Clear UX messaging; easy profile switching; audit trail |

---

## Success Metrics

| Metric | Baseline | Target | Measurement |
|--------|----------|--------|-------------|
| Invalid tool call rejection rate | ~70% (current policy) | 99.9% | `eval-runner` hallucination benchmark |
| Tool-related context tokens per task | 100% (current) | 40% (60% reduction) | Token counter in benchmark suite |
| Cross-platform test pass rate | N/A | 100% | CI matrix |
| Binary size | N/A | < 50MB | `ls -la` on release build |
| Tool call latency (validation + execution) | ~50ms | < 100ms | Instrumented traces |
| Dry-run accuracy | N/A | 100% | Unit tests |

---

## Immediate Next Steps

1. **This week:** Review and approve `ToolManifest` schema design
2. **Week 1:** Implement `ToolManifest` + `ToolRegistry` in `concerto-shell`
3. **Week 1:** Add pre-dispatch validation to `ToolExecutor`
4. **Week 2:** Port `fs.read` and `fs.write` to manifest-native versions
5. **Week 2:** Write first hallucination benchmark cases

---

*Plan scoped for Concerto v0.2.0 release. All phases assume 1-2 Rust developers working full-time. Adjust timeline based on team size and existing crate familiarity.*
