# Orchestration Studio: configuration and redesign

Studio is the global configuration workspace for the agent roster and advisory
blueprint. The Coordinator remains responsible for dispatch and completion.
The redesign separates **Agents**, **Blueprint · advisory**, and
**Configuration guide**, and gives Studio the space previously occupied by the
session quick panel. Returning to Chat restores that panel's prior state.

## What changed

- Agents are the default workspace. A compact searchable roster opens a focused
  inspector; repeated Edit and Delete buttons no longer compress every row.
- Instructions, constraints, and output format use multiline editors.
- Agents can be renamed, assigned a different role, duplicated, or removed with
  confirmation. Duplication preserves configuration with a new identity; it
  does not automatically staff the copy in a blueprint. Removal prunes draft
  staffing, relationships, and model assignments.
- Lifecycle tags accept the runtime's open vocabulary. Coverage supports custom
  tags as well as built-in stages. Unknown configured values are preserved.
- The model picker displays provider names and retains configured models absent
  from the current cache, rather than implying that they use the default.
- Blueprint order is explicitly advisory. The obsolete implication that an
  evaluation checkbox guarantees an acceptance gate has been removed.
- Validation is labelled **Configuration valid**, not **Pipeline valid**.
  Save failures retain a visible message and the draft.

## Configuration map

“Recommended in Studio” below describes the intended product boundary. It does
not mean every setting already has a Studio editor in this change.

| Area | Recommended in Studio | Current implementation / boundary |
| --- | --- | --- |
| Agent roster | Name, role, enable/disable, create, duplicate, delete | Editable; saved globally through existing per-agent files |
| Instructions | System instructions, constraints, examples, output format and typed submission contract | Editable; constraints are prompt guidance, not security enforcement |
| Agent suitability | Primary lifecycle and additional takeover coverage | Editable; custom tags round-trip; does not force dispatch |
| Models | Agent override, default inheritance, provider/model availability | Override editable; provider setup and credentials remain in Settings; cache presence is not a health check |
| Blueprint | Stage tags, labels, order, staffing, relationships, conditions, fallback persona guidance | Editable as advisory data; retains existing validation and persistence |
| Run budgets | Session dollar cap, spend multiplier, total model-dispatch cap | Runtime-backed config; the guide shows loaded dispatch cap and multiplier, read only; requires a scoped save/editor before adding controls here |
| Recovery | Subtask attempt limit, fallback model/provider, fallback enablement | Runtime-backed `multi_agent` config; recommend a future Execution section with source and next-run semantics |
| Provider resilience | Retry attempts, elapsed budget, Retry-After, timeouts | Existing provider/retry configuration; manage in Settings/config and summarize in Studio |
| Concurrency | Overall and per-provider in-flight limits | **Not currently wired:** the two `multi_agent.max_concurrent_*` fields have no runtime consumers in this `dev` snapshot. Do not expose them as working controls until wired and tested |
| Security | Agent capability requests and visibility into policy constraints | Capabilities editable here; actual policy, approvals, shell profiles, filesystem/network/environment restrictions remain in the existing Settings/config authority |
| Context | Skills, MCP, project instructions, memory and compaction | Settings/config today; recommend per-agent attachment selection only after runtime and persistence support exists |
| Observability | Decisions, tool denials, evidence, failures, progress and cost | Session runtime view; retain a separation between the editor draft and recorded run state |
| Run controls | Pause/cancel/resume and approval queue | Session controls; do not imply changing the global draft modifies an in-flight run |

## Features to retire or consolidate

- Duplicate session model/agent controls beside the global Studio editor.
- “Pipeline valid” as a claim about execution readiness or successful outcomes.
- Fixed lifecycle pickers that display a custom lifecycle as Freeform.
- Raw provider IDs where a configured display name exists.
- Legacy concurrency controls until the runtime consumes their values.
- Multiple editors for the same relationships or security policy.

Compatibility data is preserved. This UI change does not delete user-defined
agents, presets, blueprints, relationships, or unknown lifecycle tags on load.

## Settings that should not become ordinary toggles

The Coordinator's dispatch authority, policy enforcement, tool argument checks,
write gate integrity, and credential protection are invariants. Studio must
not offer a bypass. The planning hold ceilings are currently runtime constants
(`MAX_PLANNING_HOLD`, `MAX_PLANNING_HOLDS`, and
`MAX_PLANNING_RECOVERY_ROUNDS`), not configurable settings.

Before adding an Execution editor, every control needs a real runtime consumer,
validation, visible effective value and source, explicit global/project/session
scope, and a persistence path. The current Studio save writes the roster and
blueprint; it does not save the old run-tuning draft fields. Avoid presenting
those fields as editable until that contract is implemented.

## Source anchors

- `crates/desktop/src/views/orchestration_studio.rs`: state, editor and validation
- `crates/desktop/src/app.rs`: `persist_orchestration` and application layout
- `crates/config/src/schema.rs`: `MultiAgentConfig`, retry and policy schemas
- `crates/orchestrator/src/runtime_runner.rs`: spend cap and Coordinator builders
- `crates/orchestrator/src/coordinator.rs`: recovery and dispatch ownership
- ADR-59, ADR-71, ADR-74, ADR-76, and ADR-77: persistence and orchestration authority

## Verification

Run `cargo fmt --all -- --check`, then
`cargo clippy -p concerto-desktop -- -D warnings`, then
`cargo test -p concerto-desktop`. The focused state tests cover multiline prompt
edits, selection changes, cloning/model preservation, confirmation and removal
cleanup, custom lifecycles, and preservation of drafts across workspace tabs.

Native visual checks should cover 1366×768, narrow windows, long model names,
multiline prompts, empty/search-filtered rosters, invalid configuration, save
failure, and returning to Chat with the quick panel restored.
