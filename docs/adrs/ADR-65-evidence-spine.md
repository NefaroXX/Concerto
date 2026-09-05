# ADR-65: Evidence spine — facts, claims, and decisions on one append-only chain

**Status:** Accepted

Composes with ADR-58 (only the coordinator is hardcoded; everything else is
config data), ADR-60 D3/D7 (whiteboard log, approved-plan skip), ADR-63 (memory
subsystem, derived vector projections), and ADR-64 (timeline-driven, zero-waste
orchestration). Extends the ADR-60 whiteboard log from a plan-approval channel
into the **append-only evidence chain**; supersedes ADR-64's assumption that
the timeline is *only* a pure derived projection (facts must be first-class in
the log because the hot path reads them). Supersedes: ADR-64 in that narrow
sense only.

**Date:** 2026-09-04

**Deciders:** Concerto architecture + maintainer direction

## Context

Live runs (2026-09-04, accord project) established four concrete failures:

- **Cold, hallucinated DesignDoc.** The architect emitted a "vanilla HTML/CSS/JS
  chat app" design for a project whose real tree is Rust (`Cargo.toml`,
  `src/main.rs`, `src/error.rs`, `src/safety.rs`). The architect performed zero
  tool calls before emitting the doc (audit evidence: first filesystem row is
  after the doc phase). The hallucinated doc then became the enforced contract:
  claim-validation rejected reality because it was not in `proposed_files`.
- **Redundant reads.** The checkpoint `action_ledger` shows a second coder
  subtask re-reading the same paths the first coder read minutes earlier. No
  record answered "has path X been observed, and is it unchanged?".
- **Whiteboard dead in Execute mode.** `whiteboard_events` has exactly one
  writer (`plan-approved`) gated on the Apply/Replan dialog. A straight-through
  Execute run records nothing — no findings, no decisions, no command facts.
- **Deterministic fake coordination.** The planner's JSON output fails to parse
  with weak models (10 falls back in the log), the fallback pipeline
  (`design → research → implement`) is fixed code, so the "coordinator decides"
  claim is false: the same agents run in the same order on every run. Removing
  the architect/researcher does not change the *sequence*, only the roster.

Two design risks frame every decision below:

- The coordinator is itself a model and can hallucinate exactly like the
  architect did. Arbitration must therefore be **comparison against
  machine-recorded facts**, never generation of facts.
- Authoritative state must stay exact. Vector embedding is lossy and
  approximate; fuzzy truth is the drift/hallucination failure mode, not a fix.

## Decision

### 1. The whiteboard is the append-only evidence chain

Extend `WhiteboardKind` (additively, kebab-case) with `ToolExecuted` and
`WorkspaceSnapshot`. Three explicit record classes, each with a fixed
authorship boundary:

| class | written by | examples |
|-------|-----------|----------|
| **Observed fact** | runtime code only (executor, guard, indexer, snapshot) | `ToolExecuted`, `WorkspaceSnapshot`, `WriteApplied`, `WriteRejected`, `SubtaskStarted/Completed/Failed` |
| **Claim** | any model | `Finding`, `DesignDoc`, status/completion claims |
| **Decision** | coordinator policy code | continue / retry / replan / skip / replace / quarantine; reason + evidence ids |

- A claim or decision that references evidence must reference **existing**
  `event_id`s; an unknown id fails validation (append rejected).
- No model, including the coordinator, ever authors a `ToolExecuted` or
  `WorkspaceSnapshot` row. "Did the architect read a file?" is answered by
  counting facts, not by asking a model.
- The log is never summarized or deleted in place (ADR-60 D3 audit rule);
  derived views are separate.

### 2. Workspace snapshots bootstrap existing projects

Before planning begins (deterministic, language-agnostic):

1. Produce a lightweight inventory: relative paths + size + mtime (+ content
   hash where cheap). This is a **read-only** pass, no language detection.
2. Append a `WorkspaceSnapshot` event carrying the inventory and a
   `generation` id.
3. Start full vector indexing **asynchronously** (already spawned today); do
   not block planning on embeddings.
4. Inject the snapshot digest into agent context; planning waits on the
   snapshot barrier, not on embeddings.

An existing non-Concerto project therefore gets grounded inventory regardless
of language composition, and whether the vector store is warm or cold.

### 3. Observed facts are recorded on the execution hot path

Every completed command appends a `ToolExecuted` fact:

- agent id + task id + run id (attribution from the caller; never inferred)
- tool + canonical argument form (normalized, hashed)
- affected paths
- success/failure (+ exit code)
- pre/post content hashes where the tool is file-affecting
- workspace `generation` at execution time

Failures are facts too — a failed read is recorded, not retried blindly.

### 4. Resource-state fast path and safe read deduplication

Materialize a derived `resource_facts` table (migration 029) rebuilt
**forward from the log** (idempotent, recomputable — consistent with ADR-64's
derived-view rule, materialized because the hot path needs indexed lookups):

- per path: `generation`, size, mtime, content hash, last observed
  `event_id`/agent/at, dirty flag.

Read dedupe rule, applied before executing `filesystem read P`:

- `resource_facts[P]` clean and equal snapshot/observe → **serve cached
  content**, append `ToolExecuted` with `served_from=<event_id>`. The model may
  ask for a re-read; the runtime does not pay for an unchanged re-read.
- Cleanliness is the user's workspace reality: writes (`WriteApplied`),
  watcher change hints (`ReindexQueued`), and shell/git side effects dirty the
  row. If state is uncertain → execute normally (never serve stale).
- An explicit justified forced-fresh read remains possible.

A compact digest is injected into each agent's context before dispatch:
"`src/main.rs` read by coder at event 42, unchanged." Teaching the harness to
not *ask* twice is secondary to the runtime not *paying* twice.

### 5. DesignDoc is a claim with a deterministic lifecycle

```
Proposed → Verified → Active        (armed contract)
        ↘ Quarantined               (mismatch; reason is machine-checkable)
        ↘ Skipped                   (no design work needed; empty is valid)
```

- The coordinator decides whether a DesignDoc is needed at all.
- `proposed_files` become typed intents: `Create | READ | Update | Delete`.
- A **deterministic verifier** (not a model) resolves each intent against the
  snapshot + `resource_facts`: claims an Update of `main.rs` → does
  `main.rs` exist? claims Create of `index.html` → does it not exist?
  Mismatches are counted; the count + read-count of the architect are the
  quarantine reason, and both are machine numbers.
- Empty doc with a grounded snapshot = "the design is the repo" → valid when
  the coordinator determines no delta work exists.
- **Reality wins in claim validation:** a coder write to a file that *exists*
  is never rejected solely because the doc omitted it. The disk is the
  contract; the doc is a proposal.

### 6. Evidence-driven scheduling replaces the fixed fallback

Remove the hardcoded `design → research → implement` fallback shape. The
coordinator derives unmet needs from evidence gaps and chooses among the
**currently registered** agents (any stage tags; missing stages are simply not
candidates — ADR-58):

- workspace evidence missing for the objective → any exploration-capable agent
- design genuinely undecided → any design-capable agent
- evidence sufficient → implementation directly
- doc quarantined → revise / skip / proceed without a doc (coordinator
  decides; all three are valid)

Every dispatch appends a `Decision` event:
`selected_agent, reason, required_output, supporting_evidence_ids`. No agent is
called because its stage exists; an architecture doc is only consumed when it
is `Active`.

### 7. Continuation restores state, it does not replay prose

Checkpoint (schema bump) adds: whiteboard cursor (`gate_seq`), active or
quarantined doc version, snapshot `generation`, and the pending decision.
Resume compares the log since the cursor (facts appended after the
checkpoint) and chooses: continue blocked task / replace agent / skip /
refresh evidence / replan because the workspace objectively changed. Calling
the architect or researcher again is allowed — but only behind a recorded,
evidence-backed decision.

### 8. Vectors stay strictly derived

Vectorize only: source chunks, research/documentation content, and derived
summaries of log activity (`Fact`/`SessionSummary` chunks). Never
vectorize authoritative facts or decision records. The system must remain
correct with vector memory disabled entirely.

## Consequences

Positive:

- One chain answers "what is true" (facts), "what was claimed" (claims), and
  "what was decided and why" (decisions) — no more three-system divergence.
- Redundant reads and cold documents stop mechanically, independent of model
  quality.
- Agents remain removable; no language or filetype knowledge is added.
- Continuation degrades from "re-derive everything" to "resume at the cursor".

Negative / accepted:

- Migration 029 adds a derived table; the log stays the source of truth, so
  the table is rebuildable (`REBUILD` verb) and never trusted over the log.
- New `WhiteboardKind` values are additive; older binaries reading the log see
  unknown kinds and must treat them as opaque (already the case for the JSON
  payload design).
- Forced-fresh reads and dirty-on-uncertain path keep correctness but bound
  dedupe benefit on projects where the model never stops re-reading.
- `WorkspaceSnapshot` on huge trees costs an inventory walk at run start;
  hashing is opt-in per path size so the walk stays bounded.

## Acceptance criteria

1. Fresh existing polyglot project cannot produce an `Active` ungrounded
   DesignDoc (verifier must resolve every intent or quarantine).
2. Empty doc can be `Skipped` safely when the snapshot shows no delta work.
3. Immediate duplicate `filesystem read` of an unchanged path executes once
   (second serves from cache, `served_from` fact appended).
4. A write or external watcher change forces a fresh read (never stale).
5. Shell/git uncertainty invalidates caches safely (no stale serve).
6. Removing architect and researcher from the roster does not break execution;
   scheduling picks among remaining registered agents.
7. Resume does not call architect/researcher without a recorded,
   evidence-backed decision.
8. Fabricated coordinator evidence ids are rejected at append.
9. System remains correct with vector memory disabled.
10. Clippy/fmt/test green on the workspace; new tests cover 1–9.

## Implementation note (Phase 4 — §4 shipped, 2026-09-04)

ADRs document the first plan, not the final route. This note records what
actually shipped for §4 (resource fast path + safe read dedupe) so future
maintenance reads code against intent, not against an idealized spec.

### Migration 030 (content cache columns)

The derived table is `resource_facts` from migration 029. Phase 4 adds two
nullable columns via migration **030** (`content_cached TEXT`,
`content_cached_bytes INTEGER`); 030 was the next free number. The cached
content is **repudiable by rebuild**: `rebuild_from_log` replays observations
forward from the whiteboard log as ordered by migration 029, so content columns
are deliberately **not** repopulated by a rebuild — they are a hot-path
performance affordance layered over the derived view, which itself is always
rebuildable from the log (ADR-64 derived-view rule).

### Serve predicate (never-stale)

`maybe_serve_read` serves **only** when every rule holds:

1. **Plain read only.** Tool is `filesystem`, operation is `read`, and the
   canonical args contain exactly `{operation, path}` with a non-empty `path`.
   Everything else (globs, dirs, multi-file, other tools) executes normally.
2. **Row exists, is clean, and is scoped to this project root.** `resource_facts[P]`
   present within the **canonical project-root scope** (ADR-65 F5c) with
   `dirty == 0`. Rows are keyed by `(project_root_hash, path)`; a row observed
   under another root — or the legacy `''` root — is invisible and never serves.
3. **The disk agrees right now.** A fresh `std::fs::metadata` on the resolved
   path must match the row's `size_bytes` **and** `mtime_ms`. The row alone is
   never trusted; reuse of `resolve_path`/`mtime_ms` from `tool_facts` keeps the
   re-stat on the exact path the observation hashed.
4. **Cached content is self-consistent.** The cached bytes (when present)
   re-hash to the row's `content_hash` — a cache-vs-row integrity check that
   never requires an extra disk read.
5. **The policy engine explicitly allows the read.** (ADR-65 F1a) A served read
   is not a policy bypass: the side-effect-free advisory evaluation
   (`ToolExecutor::policy_verdict_is_allow`) is re-run per serve, and only an
   explicit `Allow` verdict opens the gate. `Deny`/`RequireApproval`/unknown-tool
   all fall through to the executor, where the full policy gate (and approval)
   surfaces as usual.

Any doubt degrades to normal execution — the model still receives a
byte-identical read result, the runtime just pays for it. Residual risk is
accepted precisely once: a rewrite that lands within one millisecond and keeps
both size and mtime identical is indistinguishable from "unchanged" (a
filesystem timestamp limitation, not an implementation gap).

**Escape hatches.** Serving is per-call and self-disabling:

- A `dirty` row (any write, shell/git side effect, watcher hint) never serves;
  observation stores clean rows, so the watcher's dirty marking is the safety
  switch (ADR-65 §4 "dirty-on-uncertain").
- Per-rule failures (metadata error, hash absent, cache absent, hash mismatch,
  non-`Allow` policy verdict, path escaping the root) all return serve-`None`
  and execute normally.

**Effective size bound.** Rows with `content_hash == None` never serve.
`observe_paths` hashes only content up to `MAX_HASH_BYTES`, which **aliases**
the store's `CACHE_LIMIT_BYTES` (64 KiB, ADR-65 F2b) — one shared cache bound
for both the orchestrator and the `resource_facts` store, so the guaranteed
serve bound equals the content-cache capacity (≤ 64 KiB per file).

### Cache write (hot path, exact bytes)

`cache_read_output` runs at execute sites **after** the observation (`ToolExecuted`
append) so the row exists, and stores the executor's returned
`data["content"]` — the exact bytes the model just received, with no extra disk
read. The key is the **canonical project-relative path** scoped under the
project root's hash (ADR-65 F5c/F5d), so alternate spellings of the same file
(`./x`, `a/../x`, absolute-within-root) collapse to one key, and paths escaping
the root are never cached. Store-side guards: NUL bytes are rejected (SQLite
TEXT), and content over `CACHE_LIMIT_BYTES` is rejected. A failed cache write is
logged-and-ignored: dedupe is an optimization, never a correctness input.

### Served facts

A served read appends a `ToolExecuted` fact with `served_from = <original
observation's event id>`, `success: true`, and **empty paths**. Empty paths are
intentional: a served read did not re-observe state, so recording paths would
re-clobber the row's `generation`/dirty semantics on rebuild. Served facts are
attribution and audit (Acceptance criteria 3) without pretending to be a fresh
observation.

Because the serve consumed no executor decision row, the serve additionally
persists its own **`ServedFromCache` audit row** via
`ToolExecutor::record_served_read_audit` (ADR-65 F1b): a fresh correlation id
(there is no prior decision to correlate with), `rule_matched =
"served_from_cache"`, and the served path alone in `argv` (the audit schema has
no separate path column).

### Policy re-evaluation on serve (ADR-65 F1a)

Early in Phase 4 the serve path bypassed the executor — and therefore the policy
engine — entirely. That omission is **fixed**: `maybe_serve_read` supplies a
serve candidate, but the gate opens only when the side-effect-free advisory
evaluation (`policy_verdict_is_allow`) returns `PolicyVerdict::Allow`. The
advisory path records **no decision row and consumes no quota**, so a served
read is audited as `ServedFromCache` without pretending a fresh decision was
made; a non-`Allow` verdict falls through to the executor, where the normal
`Deny`/`RequireApproval` gate applies. This is the one place a careful reviewer
should still diff against the spec: the advisory evaluation is deliberately
"allow-all or nothing" and never counts token spend.

### Action digest

`CoordinatorAgent::snapshot_digest` is now async and, when a `review_store`
pool is present, appends an `<action_digest>...</action_digest>` block after the
snapshot digest: the newest 20 observed paths — **scoped to the snapshot's
canonical project-root hash** (ADR-65 F5c) — by `observed_at DESC, path ASC`,
rendered `path | unchanged-since <event_id> | hash-<first 8 hex>` for clean
rows (hash segment omitted when no content hash was recorded) or
`path | changed` otherwise. Queried fresh on every dispatch, so agents see what
changed since planning.

**Freshness reconciliation (ADR-65 F3).** The digest does not trust the stored
`dirty` flag alone: every row's path is re-statted against the live filesystem
via the snapshot's `project_root`. A row whose size or mtime no longer matches
its observation — or whose file has vanished entirely — is folded **dirty**
(and the store's `mark_dirty` is invoked, best-effort, so the cache purge also
happens). A row that was already dirty renders `changed` as before. The
`WorkspaceSnapshotRecord` therefore carries the `project_root` it was captured
under, so the store lookup and the re-stat target the same canonical scope.

Fail-soft: absent pool, absent snapshot, or store error ⇒ bare snapshot digest
+ warning; the digest itself is not a decision input yet (Phase 2 scope).