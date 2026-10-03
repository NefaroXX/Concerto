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

## Amendment (2026-09-05) — evidence is coordinator context, not a compiled dispatcher

Revised **in place** (per the project owner's standing instruction, no new ADR
number). §6's intent — "the coordinator derives unmet needs from evidence gaps
and chooses among the currently registered agents" — is retained, but the
implementation over-built it into a compiled decision function
(`evidence_scheduler` rules (a)–(f)) that became a pipeline authority. **The
scheduler is removed**: no compiled rule selects an agent. Evidence (facts,
claims, decisions) is injected into the Coordinator's context as guidance; the
Coordinator decides via the policy-gated `call_specialist` tool (ADR-35
amendment 2026-09-05).

**Unchanged (killed by nothing in this amendment):** every dispatch appends an
evidence-backed `Decision` event (`selected_agent, reason, required_output,
supporting_evidence_ids`); fabricated evidence ids are rejected at append
(acceptance 8); removing agents from the roster only removes them from the
Coordinator's context (acceptance 6); resume (§7) restores from the ledger at
the cursor, and calling architect/researcher again remains allowed only behind
a recorded, evidence-backed decision (acceptance 7).

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

## Implementation note (Phase 8 — §8 shipped, 2026-09-05)

What actually shipped for §8 (vectors stay strictly derived):

- **Audit.** Two runtime producers embed session/log-derived content into
  vector chunks: the D6 consolidation projection (`Fact` chunks) and the
  agent-loop task summary (`SessionSummary`). The consolidation projection
  previously embedded decision `reason` text verbatim (and, defensively, the
  whole payload JSON when no known key matched) — a violation of the §8 rule
  that decision records are never vectorized. It now embeds **aggregate-only**
  text: per-author decision counts, selected-agent outcome distributions,
  approval counts with the newest gate sequence, and resolved review status
  distributions. No reason, `required_output`, evidence id, artifact hash, or
  raw payload JSON reaches an embedding — authoritative records live only in
  the log. Fold windows whose aggregates are identical converge to the same
  chunk id (idempotent upsert) instead of minting spurious supersessions.
- **Retention.** `TtlManager::prune_derived_summaries` prunes `Fact` +
  `SessionSummary` vector rows only (source chunks untouched): a per-session
  count cap (`memory.summary_keep_per_session`, default 20, 0 keeps all) and
  an age window (`memory.summary_retention_days`, default 365, 0 disables the
  window). Session buckets come from the chunk metadata sidecar's
  `session_id` (consolidation projections stamp the folded window's session
  id); rows without attribution retain project-wide in one shared bucket.
  Removal is a hard delete of vector rows plus best-effort FTS deletion,
  idempotent, CancellationToken-aware, and logged.
- **Disabled store (acceptance 9).** Vector memory was already optional on
  every Phases 1–7 path (snapshot barrier → `resource_facts`, read dedupe,
  verifier, scheduler, resume — none reference the vector store; the
  consolidator is only constructed when memory is enabled). The remaining
  hard edge was fixed: a memory-system init failure during a run now degrades
  to `NullMemoryStore` with a warn instead of aborting the run. Integration
  tests prove the continuation loop completes with a forbid-write memory
  store wired in.

## Implementation note (Phases 1–7 + remediation, 2026-09-05)

What actually shipped for the remaining phases (Phases 4 and 8 have their own
detailed notes above):

**Phase 1 (evidence kinds + resource_facts table).** `WhiteboardKind` was
extended additively with `ToolExecuted`, `WorkspaceSnapshot`, and `DesignDoc`
kinds. The `resource_facts` derived table was added via migration 029 (plus
030 for content-cache columns and 031 for per-root scoping). The table is
rebuildable forward from the whiteboard log; content-cache columns are a
hot-path affordance not repopulated by rebuild. Commits: `f276086`, `2b80df5`.

**Phase 2 (workspace snapshot readiness barrier).** `workspace_snapshot.rs`
produces a lightweight inventory (relative paths + size + mtime + optional
content hash) before planning begins, appends a `WorkspaceSnapshot` event, and
blocks dispatch until the snapshot is available. Vector indexing remains
async. The snapshot digest is injected into agent context. Commit: `e904a3b`.

**Phase 3 (tool-level fact writer).** `tool_facts.rs` records a
`ToolExecuted` fact on every completed command with agent attribution (agent
id, task id, run id), tool + canonical args, affected paths, success/failure,
pre/post content hashes, and workspace generation. The fact writer is the
single producer; no model ever authors `ToolExecuted` rows. Commit: `748c3d5`.

**Phase 5 (DesignDoc verifier).** `design_doc_verifier.rs` implements the
deterministic lifecycle `Proposed → Verified → Active | Quarantined | Skipped`.
Each proposed-file intent (Create/Read/Update/Delete) is resolved against the
snapshot + `resource_facts`. Mismatched claims are counted; the count plus the
architect's read-count form the machine-checkable quarantine reason.
Objective-path sufficiency is approximated by `snapshot_present ∧
in-scope-facts` (the full objective-path metric from the ADR is deferred but
safe because hallucinated docs quarantine deterministically). Commit: `cbf0ecb`.

**Phase 6 (evidence-driven scheduling).** `evidence_scheduler.rs` replaces
the hardcoded `design → research → implement` fallback. The coordinator
derives unmet needs from evidence gaps and dispatches among registered agents.
Every dispatch appends a `Decision` event with selected agent, reason, required
output, and supporting evidence ids. Decision-event append validation rejects
fabricated evidence ids. Commit: `b38d6b5`.
*(Superseded by the 2026-09-05 amendment above: the scheduler module is
removed; dispatch authority is the Coordinator's `call_specialist` tool, and
the Decision-event discipline — including append validation — is kept.)*

**Phase 7 (continuation + resume).** `resume.rs` restores state at the
whiteboard cursor. Checkpoint schema bumps to v4 (whiteboard cursor, active
doc version, snapshot generation, pending decision). Resume compares the log
since the cursor and chooses: continue, replace agent, skip, refresh evidence,
or replan. Resume never dispatches architect/researcher without a recorded,
evidence-backed decision. WriteApplied→dirty wiring is deferred but safe via
serve/digest re-stat (the action digest re-stats every row against the live
filesystem). Commit: `c3e0646`.

**Security remediation (F1–F5).** Canonical per-root keys (`project_root_hash`
scoping) prevent cross-root cache poisoning. Serve gates re-evaluate policy
via `policy_verdict_is_allow` (ADR-65 F1a). Fresh digest re-stats rows
against the live filesystem before rendering the action digest (ADR-65 F3).
Content purge on dirty removes cached bytes. The serve path was hardened from
an initial implementation that bypassed the policy engine — that omission is
fixed: `maybe_serve_read` supplies a candidate, but the gate opens only on an
explicit `Allow` verdict. Commits: `0c5c14e`, `1bd993a`, `781505d`, `3e09dfa`.

## Addendum (2026-10-03) — world-model projection rules and non-goals (#144)

**Status: PROVISIONAL — covers rules through #142.** This addendum amends nothing
above; it records the Coordinator's world-model rules (issue #56, parent #51) in
this ADR, because they were previously documented only in source. The normative
source remains the module docs of `crates/orchestrator/src/world_model.rs`; if
this addendum and that module doc ever disagree, the module doc wins and this
section is corrected.

**Merge state (read before relying on the "(#13x)" markers).** Rules tagged
*(#135)*–*(#142)* are authored in **open PRs and are not on `dev`**. `dev` today
implements only the F-\*, A-\*, V-\* and Q-\* rows marked "on `dev`" below. The
ids are recorded now so the whole rule set is reviewable in one place, and so
these rules are not re-proposed as a redesign later. This addendum is
**finalized (markers dropped) once the Phase-2 PRs merge**; until then it
describes intent-as-specified, not shipped behavior.

**Standing invariants for all world-model work** (from the #134 audit, restated
here because every rule below is subject to them):

- `WorldModel::build` stays **pure and deterministic** — same input, same model
  and same render; no model calls, no I/O, no randomness.
- **No new store.** The projection reads state the Coordinator already holds
  (ADR-65 event window, `resource_facts` rows, decision journal, failure
  diagnoses, roster, ledger artifact paths, checkpoint generation signals).
- New serialized fields are **additive** (`#[serde(default)]`); old checkpoints
  and old logs keep working.
- **Anything dropped must be observable** (see the drop-observability rule).

**Relationship to the 2026-09-05 amendment.** Unchanged and reinforced: the
world model is **context, not authority**. No rule here selects an agent, opens a
write gate, or compiles into a dispatcher; dispatch remains the Coordinator's
policy-gated `call_specialist`, and every dispatch still appends an
evidence-backed `Decision` event.

### Fact freshness (`FactStatus`)

`WorldFact` is referenced **by id** (`ref_id` + short bounded label + optional
`artifact` + derivation `seq`); full prose is never copied in.

| Rule | Statement | Issue | State |
|------|-----------|-------|-------|
| **F-VERIFY** | A fact derived from a successful `WriteApplied`, or a successful file-affecting `ToolExecuted` fact, is `Verified` — it quotes an executed observation. | #56 | on `dev` |
| **F-ASSUME** | A fact derived from a `Finding` or `DesignDoc` event (an assertion with no executed observation behind it) is `Assumed`. | #56 | on `dev` |
| **F-SUPERSEDE** | A fact naming an artifact path is `Stale` when the event window holds a **newer effective write** to that path (higher `gate_seq`) than the fact's derivation event — the artifact moved on after the fact was derived. Non-write executions record the path they **observed** (reads), so they are covered by the same path-level rule. | #56, extended by #139 | on `dev`; read-artifact arm pending #139 |
| **F-WORKSPACE-SUPERSEDE** | A fact naming **no** artifact (a pathless build/test/check execution, which observed the workspace as a whole) is `Stale` when the window holds **any** effective write newer than the fact. There is no path to key on, so any later write moves the workspace on. | #139 | pending |
| **F-GENERATION** | When the resume workspace-change verdict fired (checkpoint generation ≠ current snapshot generation), every log-derived fact drops to `Stale` until a fresh observation re-verifies it — the conservative reset (ADR-65 §7). | #56 | on `dev` |

**Labels stay observations.** A fact's label states what was *executed* ("ran
`cargo test`", "read `src/main.rs`"), never an outcome claim. "Ran the tests" is
`Verified`; "the tests pass" is not a fact this projection can mint.

**Supersession is positional, not sticky.** F-SUPERSEDE is decided by
`gate_seq` ordering alone, so a later *successful* write re-verifies what it
touches. There is no monotonic "disproved" state — see open question 1.

### Artifact classification (`ArtifactStatus`)

| Rule | Statement | State |
|------|-----------|-------|
| **A-DIRTY** | A `resource_facts` row with `dirty = true` marks its artifact `Dirty` (uncertain, never verified-clean). | on `dev` |
| **A-GENERATION-ROW** | A row whose recorded generation differs from the current snapshot generation marks its artifact `Stale`. | on `dev` |
| **A-CLEAN** | A clean row whose generation matches (or with no current generation known) marks its artifact `Clean`, owned by the row writer. | on `dev` |
| **A-WRITTEN** | A path known only from effective write events or the ledger's file list (no observation row) is `Written`, owned by the newest write's agent. | on `dev` |
| **V-CHANGE** | While the resume workspace-change verdict stands, **no** artifact is verified-clean — the deterministic `artifact_verified_clean` query demands a workspace that has not changed materially since the checkpoint. | on `dev` |

A resource-fact **row** (`observed = true`) is the only source that can clear
`Dirty`; write attribution (`observed = false`) carries ownership, and its
`dirty` field is ignored by design.

### Unresolved questions (`Q-*`)

Questions are the projection's only carried progress: a small ledger
(`MAX_OPEN_QUESTIONS = 12` open, `MAX_RESOLVED_QUESTIONS = 8` remembered),
checkpointed additively so a resume restores — or, for old checkpoints, rebuilds
— the same model.

| Rule | Statement | State |
|------|-----------|-------|
| **Q-OPEN-PROBLEM** | A failure diagnosis requiring replanning, or a non-retryable unviable-retry one, opens an `OpenProblem`. | on `dev` |
| **Q-OPEN-MISSING** | A journal decision that stood `Rejected` opens a `MissingEvidence` (the work needs a corrected decision). | on `dev` |
| **Q-OPEN-AMBIGUOUS** | A stale pending dispatch decision opens an `AmbiguousRecovery`. | on `dev` |
| **Q-OPEN-BLOCKED** | An artifact with status `Dirty` opens a `BlockedPath`. | on `dev` |
| **Q-DEDUPE** | A rebuilt question matching a standing question by stable key does **not** open again — the standing entry survives (one id, growing age) until resolved. | on `dev` |
| **Q-RESOLVE-LINKED** | A **linked** `OpenProblem`/`MissingEvidence` question (one carrying a `subject_decision_id`) resolves only on a `Settled` journal decision recorded at or after the question's opening journal length that is **related to its subject**: the decision is the retry/replacement/reconsider descendant of the subject (its Freeze payload names the subject decision), or its `expected_artifacts` touch the question's blocked path. An unrelated parallel settle is **coincidence, never resolution**. (#135) | pending |
| **Q-RESOLVE-LEGACY** | *Transitional.* A `MissingEvidence` question restored from a pre-#135 checkpoint carries no subject (new code always records one), so it keeps the old any-settled-decision rule until it resolves and cycles out of the ledger. Documented transitional behavior, not a license to resolve by coincidence. (#135) | pending |
| **Q-RESOLVE-UNLINKABLE** | An `OpenProblem` with no decision subject (`FailureDiagnosis` carries no task/decision/path linkage yet — the add-linkage follow-up) never resolves by settle: it **stands and ages** (`cycles_open`) rather than resolving by coincidence. (#135) | pending |
| **Q-RESOLVE-AMBIGUOUS** | The pending dispatch decision was cleared. | on `dev` |
| **Q-RESOLVE-BLOCKED** | A newer CLEAN observation of the path (an observation event id different from the one the question was opened against) resolves it. | on `dev` |
| **Q-RESOLVE-STAY** | A resolved question is never re-opened. | on `dev` |
| **Q-PERSIST** | Open questions persist across rebuild cycles until one of the Q-RESOLVE rules fires, independent of whether the opening signal still shows. | on `dev` |

**Why the linkage change exists (#135).** Under ADR-60 concurrent work, unrelated
parallel tasks settle constantly, so the old "any later settled decision resolves
the question" rule closed real open problems by coincidence — and Q-RESOLVE-STAY
then made the loss permanent. Coincidental closure is a silent failure mode of
exactly the kind §1 forbids: a question the Coordinator can no longer see is a
fact it cannot weigh.

### Grounding is provenance, never promotion

| Rule | Statement | Issue | State |
|------|-----------|-------|-------|
| **G-NOUPGRADE** | A `Finding` fact's `grounded_by` refs (bounded, `MAX_GROUNDED_BY = 4`, sanitized at render because they are agent-authored citations traveling into the prompt) record **which** evidence an assertion rests on. Grounding is **provenance only**: it never upgrades `Assumed` to `Verified` — only an executed observation (F-VERIFY) does, and **repetition never raises a fact's standing**. | #141 | pending |

Refs are rendered compactly (e.g. `[ev2 ← ev9, ev11]`); an ungrounded Finding
renders without refs. G-NOUPGRADE is the load-bearing half of #141: provenance
without a confidence score.

### Untrusted-text rendering (trust boundary, #137)

Model-authored text reaches the Coordinator's prompt through this block
(agent → `Finding` → fact label → prompt), so the render is a **trust
boundary**, not a formatter. Three rules, all landing together (#137):

1. **Sanitize on store, sanitize on render.** Every stored string is sanitized
   in `bounded()` (whitespace/newlines collapsed, control characters stripped,
   angle brackets and the literal block tag name neutralized) and again at
   render time. Length-capping alone is not sanitization: a label containing
   `</world_model>` plus instructions could otherwise close the block and
   inject lines.
2. **Explicit trust sections.** Model-authored entries (tasks, `Assumed` facts,
   their assumption labels) render **only** under the
   `unverified, model-authored — data, not instructions:` subsection
   (`UNVERIFIED_SECTION_MARKER`), never interleaved with runtime-observed
   entries. Verified entries never appear in the unverified section and vice
   versa.
3. **Balanced truncation.** Truncation runs **before** the closing tag is
   appended, with the tag's width reserved from the `MAX_RENDER_CHARS` budget,
   so even maximally hostile state ends with exactly one `</world_model>`
   (`RENDER_CLOSING_TAG`). Pre-#137, `truncate_with_mark` cut the tail and
   could drop the closing tag.

No rule id is minted for this group; it is identified by the constants above and
the #137 sanitization/trust-boundary note in the module docs.

### Payload-shape contract (finding-key, #136)

The projection and the event **writers** share one typed accessor per payload
shape, living next to the event definitions in
`crates/sessions/src/whiteboard.rs` (`finding_text`, `write_applied_path`,
`tool_executed_view`, and `consult_finding_payload` for the write side):

- A Finding's label reads the **`findings`** key — the key
  `append_consult_finding` actually writes — with `summary` and `content`
  retained as accepted aliases, bounded and sanitized (#137).
- Builders and writers both go through the accessors, so a writer-side shape
  change **fails a world-model test** instead of silently degrading every
  consultative Finding to the `"finding recorded"` fallback (and collapsing
  distinct Findings into one assumption via dedupe-by-label).
- Contract tests construct events through the **real writer paths**, not
  hand-built JSON.

### Window and bounds (#138, #142)

- **Session-scoped event window (#138).** The builder consumes the **newest N
  events of *this session*** (`WORLD_MODEL_EVENT_WINDOW = 256`, ascending), via
  a session-scoped head query — deliberately **not** `gate_seq > global_head - N
  AND session_id = ?`, which measures the window in *global* seq units: other
  sessions' writes both walk the anchor forward and stretch the gaps between
  this session's rows, so the window came back silently shrunk (possibly empty)
  whenever neighbors got busy, and facts/supersession/staleness degraded with
  no signal. Fail-soft: no pool, a cancelled token, or a read failure yields an
  empty window — a projection must never fail the dispatch loop.
- **Pinned fact slots (#142).** Of the `MAX_WORLD_FACTS = 32` slots, at most
  `MAX_PINNED_FACTS = 8` are reserved for facts about artifacts active work
  depends on (a non-terminal decision's `expected_artifacts`, or an open
  question's `blocks` path); the rest fill newest-first. A burst of irrelevant
  reads therefore cannot evict the facts the Coordinator is actively relying
  on. This is **deterministic pinning by membership, never a weighted relevance
  score**, and the total stays within the cap with deterministic ordering.

Other bounds (all on `dev`): `MAX_WORLD_TASKS = 24`, `MAX_WORLD_ARTIFACTS = 32`,
`MAX_WORLD_ASSUMPTIONS = 8`, `MAX_WORLD_RISKS = 8`, `MAX_WORLD_AGENTS = 16`,
`MAX_WORLD_MODELS = 8`, `MAX_WORLD_CRITERIA = 8`, `MAX_LABEL_CHARS = 120`,
`MAX_OBJECTIVE_CHARS = 200`, `MAX_RENDER_CHARS = 4_000`. Every list is capped
and every label is length-bounded, so prompt cost is pinned, never proportional
to log length.

### Drop observability (attribution risk, #140)

A drop that is not counted is a silent failure of the recurring kind in this
project. `extract_writes_and_facts` returns an `Extraction` that **counts**
write events carrying no usable `input.path`, and `build` turns a non-zero
count into (a) one `tracing::warn!` naming the count and the first dropped
event id, and (b) a bounded risk entry — "N write event(s) could not be
attributed — freshness may be overstated" — so the Coordinator sees that its
own freshness signals may be overstated instead of inheriting a quietly
under-firing F-SUPERSEDE.

Scope discipline (deliberate):

- Only an unattributable **`WriteApplied`** counts. A failed execution applied
  nothing, so it is not a drop of this kind.
- A file-affecting `ToolExecuted`'s post-hoc `paths` are not counted: a
  `delete_file` legitimately reports none, and the write is already attributed
  by the gate's own `WriteApplied` row.
- **Unknown event kinds stay silently ignored** — the established log-compat
  convention (older/newer readers treat unknown kinds as opaque), not a defect.

### Non-goals (explicit — do not re-propose)

These are recorded to stop the same redesign recurring. A proposal that violates
one of these is a **new decision requiring a new ADR**, not an implementation
detail of this one.

1. **No confidence scalars.** Facts carry a discrete `FactStatus`
   (`Verified` / `Assumed` / `Stale`, plus proposed `Contradicted` — see open
   question 1) and bounded evidence ids. No float, score, probability, weight,
   or "how much do we believe this" field. Grounding refs are provenance, never
   a score (G-NOUPGRADE); pinning is membership, never relevance (see #142).
2. **No persistent cross-run claim store.** The projection is rebuilt from
   state the run already holds and is **not** a claims database. The only
   persistence is the checkpointed `WorldModel` (`checkpoint.rs`, additive,
   round-trip pinned), which exists so a **resume of the same run** restores
   the question ledger at the cursor (ADR-65 §7). Nothing accumulates claims
   across runs, and the world model itself writes nothing new to
   `resource_facts`, the event log, or any side table.
3. **No embedded claims.** Vectorized content stays strictly derived (ADR-65
   §8): source chunks, research/documentation, and aggregate-only projections of
   log activity. Authoritative facts, decision records, evidence ids, and
   artifact hashes are never embedded, and decision `reason` text never reaches
   an embedding. A world-model fact is **referenced by id**, never copied as
   prose into an index.
4. **No LLM-maintained world state.** `WorldModel::build` is a pure function
   over existing deterministic signals. No model authors, edits, repairs, or
   "reconciles" the model, and no model-authored text is promoted to `Verified`
   by being believed more than once. The Coordinator *reads* the projection and
   decides through the policy-gated `call_specialist`; it never writes the
   projection (consistent with ADR-65 §1's authorship boundary: only runtime
   code authors facts, only policy code authors decisions).

## Open questions (recorded, **not** settled rules)

1. **#143 spike — contradiction is recommended, not adopted.** The spike
   concluded **implement**, tight scope: add `FactStatus::Contradicted` with
   rule **C-FAIL** (an `Assumed` fact naming path P is contradicted by a later
   *failed* execution touching P). **Not implemented and not a rule.** Spike
   findings worth carrying forward:
   - `FactStatus` **is** persisted — inside the checkpointed `WorldModel`, not
     in `resource_facts` or the event log (status is derived at build time).
     This corrects #143's step-0 assumption; a fourth variant therefore touches
     every exhaustive consumer (staleness fold, assumptions filter, render
     counts and per-fact render, checkpoint fixture, module-doc rule list).
   - Failed `ToolExecuted` facts exist in the log (tool, canonical args,
     `success: false`, `exit code`, paths, attribution) but carry **no error or
     output string** — a contradiction can cite *that* a check failed, not *why*.
   - Contradiction must **reduce standing, not flip a boolean**: both the claim
     and the contradicting observation stay visible (a bounded
     `contradicted_by` annotation, same id hygiene as `grounded_by`); F-SUPERSEDE
     stays positional, so a later successful write un-contradicts (never
     sticky); `Contradicted` facts must not feed `assumptions` nor
     `artifact_verified_clean`.
   - Display/truncation precedence: Verified > Contradicted > Assumed > Stale,
     and contradiction annotations must rank **below** Verified writes so
     disproof never evicts proof — which interacts with the #142 pinned slots.
   - Follow-ups raised by the spike, also unimplemented: record a bounded
     `stderr_tail` on failure payloads (the missing error text is the main
     evidence gap), and confirm the #142 pinning rank against contradiction.
2. **#135 open design question — dismissing unlinkable questions.** A question
   with no linkable resolver can stand indefinitely and compete for the 12-slot
   `MAX_OPEN_QUESTIONS` cap (Q-RESOLVE-UNLINKABLE). Whether the Coordinator
   needs an explicit way to dismiss one is **undecided**; if it exists it must
   be a journaled decision, never a compiled rule (ADR-71). Unresolved as of
   this addendum.
3. **#135 split follow-up — add linkage.** `FailureDiagnosis` still carries no
   task/decision/path linkage, so `OpenProblem` questions remain unlinkable by
   construction. Recording that linkage is the prerequisite for retiring
   Q-RESOLVE-UNLINKABLE, and is tracked as the "add linkage" half of #135.

### Provenance

- Issues: #56 (world model), #134 (audit parent), #135–#142 (rules above), #143
  (spike, open question 1), #144 (this addendum).
- Source of truth for rule text: module docs of
  `crates/orchestrator/src/world_model.rs`; event-payload accessors in
  `crates/sessions/src/whiteboard.rs`; event-window loading in
  `crates/orchestrator/src/coordinator.rs` (`load_world_model_events`);
  checkpoint persistence in `crates/orchestrator/src/checkpoint.rs`.
- Compose with: ADR-71 (coordinator supremacy — no compiled authority),
  ADR-64 (derived views), ADR-60 D3 (append-only audit log).