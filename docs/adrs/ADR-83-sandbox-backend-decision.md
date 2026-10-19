# ADR-83: Sandbox backend decision — portable capability interface, no checkpoint backend

**Status:** Accepted 2026-10-12 (H10 harness-upgrade milestone).
**Date:** 2026-10-12
**Deciders:** Concerto architecture
**Related crates:** `concerto-core` (capability vocabulary), `concerto-orchestrator`
(preflight seam, `environment_describe` reporting)
**Supersedes:** nothing. **Extends** ADR-72 (containerized profile enforcement;
§5 Windows gate in force, untouched) and ADR-82/H01 (contract-surface
vocabulary; H09 restore-coverage preflight). **Preserves** ADR-71 (no
coordinator change — nothing here grows `coordinator.rs`).

Implements H10 verbatim: *"Define the portable capability interface first.
Evaluate a container/runtime checkpoint backend only after H08/H09 demonstrate
the needed semantics."*

## Context

The sandbox surface before this decision was enforcement without
description. ADR-72 made `SandboxProfile::Containerized` real — opt-in
docker/podman `PATH` detection (`crates/core/src/sandbox.rs`), a fail-closed
policy admission gate (`crates/core/src/policy.rs:542-579`), shell routing
through `<runtime> run`, and the required `CommandRouting::Containerized`
producer marker (`crates/core/src/types.rs:455-461`) — but no caller can ask
"what isolation is actually in force for this profile on this platform, and
what does the sandbox restore" as one bounded, versioned answer. The H09
restore-coverage model (`crates/core/src/checkpoint.rs:1096-1157`:
`EffectKind`, `EffectSupport`, `preflight_rollback_guarantee`) answers "what
can this rollback reverse" across file/database/process/shell/network effects
BEFORE execution, with shell and network unclaimed by any adapter — but the
sandbox backend had no offer-shaped share in that preflight, so its
non-coverage was structural silence rather than a declared report.

What H08/H09 demonstrated, and why a checkpoint backend is evaluable only
later:

- H08 (experiment workspaces): managed workspaces with explicit review and
  promotion — the workspace lifecycle a sandbox checkpoint would have to
  scope, now with a real owner and promotion path.
- H09 (restore-coverage preflight + managed adapters): every rollback states
  its coverage BEFORE executing (`assemble_rollback_offers` /
  `check_preflight_for_execution`,
  `crates/orchestrator/src/managed_rollback.rs:104-175`); the database
  adapter restores scoped rows only and refuses schema drift; the process
  adapter restarts without memory state (`memory_state_restored: false` by
  construction); shell and network effects are refused, never partially
  executed. The honest-coverage discipline — strongest-claim-wins merge,
  qualified-only-with-acknowledgement, refused-never-runs — is exactly the
  bar a future sandbox checkpoint backend would have to clear per effect
  kind, with failure-injection proof per claim.

Constraints binding this decision: everything keeps its normal policy/audit
path through `ToolExecutor` (constraint 1); a container with a host bind
mount is not a rollback system (constraint 5); long operations stay
cancellable and bounded behind a service seam, never in `coordinator.rs`
(constraint 7).

## Decision

### 1. The portable capability interface ships; the checkpoint backend does not

`concerto-core::sandbox` gains a backend-neutral, versioned
`SandboxCapabilityDescriptor` (`crates/core/src/sandbox.rs:403`) — one
sandbox profile × every platform — expressing:

- **what isolation is actually in force, if any** (`SandboxIsolation`,
  `:325`): `Unconfined` for `None` and for the denied stub profiles (never
  admitted, so no isolation is in force), `Containerized { runtime }` for the
  container profile. The honest "none" case is a first-class variant, not an
  omitted field.
- **declared resource coverage as H09 offers** (`coverage: Vec<EffectCoverage>`,
  one entry per `EffectKind`): every kind `Unsupported` in this milestone,
  each with its own reason — the sandbox banks no pre-images and reverses no
  file/database/process/shell/network effects. A descriptor feeds
  `preflight_rollback_guarantee` through `offers()` / `offer_for()` (`:426`);
  the verdict logic is never duplicated (adaptation, not a fork).
- **per-platform availability as explicit tri-state** (`PlatformAvailability`,
  `:278`): `Available` / `Unavailable` / `UnsupportedByDecision` — never a
  default that silently reads as "fine".
- **what the backend explicitly cannot restore** (`cannot_restore`,
  `UnrestorableResourceKind`, `:354`): mounted host data, remote services,
  and anything outside the container's own writable layer — named in the
  interface, with reasons, for every profile.

The interface is descriptive only: the policy engine never reads it, there
is no conversion into any policy type, and naming a mechanism grants
nothing.

### 2. No container/runtime checkpoint-restore backend ships in this milestone

Stated plainly, because it is the point of the milestone: **full sandbox
restoration is an optional later capability, not a promise of this one.**
The sandbox declares isolation only and restores nothing
(`NO_SANDBOX_RESTORE_MILESTONE`, `crates/core/src/sandbox.rs:274`). A
checkpoint backend is evaluated only after the evidence in §Residuals exists.

### 3. Windows stays unsupported via the ADR-72 §5 gate — not reopened

Every descriptor's `windows` field is `UnsupportedByDecision` citing
`ADR-72 §5` (`SANDBOX_WINDOWS_DECISION`, `:247`), for every profile. The
decision is the answer: the probe is never consulted for Windows (a probe
would be a lie — proven by test with a FOUND runtime still reporting the
decision). This ADR does **not** supersede, amend, or choose between the
residual options ADR-72 §5 records; `docs/DEFERRED.md` row 36 keeps its
re-entry condition (a superseding ADR choosing (a), (b), or (c)) intact and
is referenced, not touched.

### 4. Linux/macOS reporting is explicit and claims parity only where implemented

`linux`/`macos` derive from the same supplied probe — the implementation
shares one `PATH`-probe mechanic on both, so the same verdict on both is
honest parity, not assumed parity. The `Available` detail states the
mechanism's ceiling (daemon/image health verified only at invocation,
fail-closed). Anything differing at spawn time (notably the macOS
Docker-Desktop VM layer beneath an identical probe) is a residual, not a
reported difference.

### 5. Container deletion/recreation does not restore mounted host data or remote services

The ADR-72 constraint-5 point ships as a stated limitation, not a caveat
(`CONTAINER_RECREATION_LIMITATION`, `:251`): deleting and recreating the
container drops the container's own writable layer only; host paths
bind-mounted into the container survive removal and remote state is never
reversed. A container with a host bind mount is not a rollback system.

### 6. The per-platform table as it actually behaves

(Probe `Unavailable`; with a found runtime the two `Containerized` POSIX
cells read `Available` instead — Windows never changes.)

| Profile | Isolation in force | Linux | macOS | Windows | Restore coverage |
|---|---|---|---|---|---|
| `None` | unconfined (ambient authority) | `Unavailable` — no isolation declared | same as Linux | `Unsupported-by-decision` (ADR-72 §5) | none — all kinds `Unsupported` |
| `ReadOnlyFs` | unconfined (denied stub, never admitted) | `Unavailable` — `sandbox_profiles_not_implemented` | same as Linux | `Unsupported-by-decision` (ADR-72 §5) | none |
| `NetworkIsolated` | unconfined (denied stub, never admitted) | `Unavailable` — `sandbox_profiles_not_implemented` | same as Linux | `Unsupported-by-decision` (ADR-72 §5) | none |
| `Containerized` | containerized (runtime, or the miss named) | `Available` iff runtime on `PATH`, else `Unavailable` naming `sandbox_containerized_runtime_unavailable` | same as Linux (shared probe mechanics) | `Unsupported-by-decision` (ADR-72 §5; probe not consulted) | none — writable-layer ephemerality is not file restore |

### 7. Wiring (additive only — no semantic change)

- H09 preflight: `assemble_rollback_offers_with_sandbox(file_record,
  sandbox_offers)` (`crates/orchestrator/src/managed_rollback.rs:118`)
  merges a descriptor's `offers()` strongest-wins; `assemble_rollback_offers`
  (`:104`) delegates with an empty share, so every existing verdict is
  byte-identical. The all-`Unsupported` sandbox share can neither upgrade nor
  downgrade another adapter's claim (proven by test).
- `environment_describe`: every rendering carries the run's descriptor
  (`describe_sandbox_capability`,
  `crates/orchestrator/src/environment_describe.rs:331`) computed pure from
  the registration-time cached probe (`DescribeFacts::sandbox_runtime`,
  `:195`; one `PATH` scan per process, zero per-call probes, deliberately
  unfingerprinted observation). The `EnvironmentManifest` schema is
  untouched — reporting lives in the describe payload, not the manifest.

## Consequences

- Operators get one bounded, versioned answer to "what is the sandbox doing
  here" instead of enforcement silence; `environment_describe` is the read
  path, through the normal policy/audit gate like every other tool.
- Nothing becomes restorable that was not: preflight verdicts are unchanged,
  the fail-closed container admission gate is untouched, `coordinator.rs` is
  untouched, no dependency is added, no migration is needed (next free would
  be 041 — none added).
- The H09 review carry-forward folded in alongside (unconditional
  registry-entry removal once a managed-process reap begins;
  `crates/orchestrator/src/managed_process.rs:149-178`) keeps timeouts
  honest (`StopFailed`) while making ids reusable — a stuck unreaped handle
  can no longer block re-registration.
- Costs / risks: one more describe payload key (bounded, fixed-size shape);
  the registration-time runtime observation can go stale if a runtime is
  installed mid-session (the policy path already has `refresh()` for that;
  describe documents the staleness, see Residuals).

## Verification

- `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --
  -D warnings` clean.
- `cargo test -p concerto-core` (capability tests:
  `crates/core/src/sandbox.rs` `capability_tests`); `cargo test -p
  concerto-orchestrator --lib` (managed_process reap, managed_rollback
  merge, environment_describe reporting); integration
  `vertical_slice step1` (descriptor on the executor path), plus the
  `file_checkpoints` / `experiment` suites for preflight-adjacent regress.
- Failure-injection coverage (canned probe results — **no test requires a
  real container runtime**; the existing fake-`docker`-on-`PATH` pattern
  stays with the probe tests, capability tests are pure):
  - unconfined reports `Unavailable` with a reason and preflight over its
    offers refuses shell/network (`unconfined_profile_reports_unavailable_and_refuses_shell_network`;
    describe-level: `describe_reports_the_sandbox_capability`).
  - absent/malformed runtime is fail-closed, naming
    `sandbox_containerized_runtime_unavailable`
    (`unavailable_runtime_is_fail_closed`; probe-level coverage pre-exists
    and is extended, not duplicated).
  - Windows returns `Unsupported-by-decision` with a FOUND runtime passed in
    — proving the probe is not consulted
    (`available_runtime_is_honored_on_linux_macos_but_not_on_windows`;
    describe-level: `describe_honors_a_found_runtime_without_probing_windows`).
  - mounted host data / remote services / outside-writable-layer are named
    for every profile and asserted (`declared_unrestorable_resources_are_named`;
    describe-level `cannot_restore` kinds; `vertical_slice step1` pins two).
  - sandbox offers cover every kind and restore nothing
    (`sandbox_offers_cover_every_kind_and_restore_nothing`); the merge is
    verdict-neutral (`sandbox_share_never_upgrades_another_adapters_claim`).
  - reap timeout/cancel arms report honestly via an injected pending wait
    (`reap_timeout_reports_stop_failed`, `reap_honors_cancellation`); the
    pre-attempt boundary retains the entry
    (`pre_attempt_cancel_keeps_the_entry_for_retry`); cancellation of
    describe still fails closed before collection
    (`cancelled_describe_never_collects`, pre-existing).
- What is NOT run: the full workspace suite (change is additive to two
  crates; impacted suites above are the blast radius), any real-runtime
  test (none exists by design).

## Residuals

Recorded so the boundary of this decision stays explicit.

- **Checkpoint-backend evaluation is deferred, with its evidence bar
  stated.** A future backend must, per effect kind it claims: name the
  reversal method, declare `Supported`/`Qualified`/`Unsupported` through the
  H09 offer shape, and prove each claim by failure injection — including an
  adapter for shell/network effects (unclaimed today, refused by
  construction) or an explicit, acknowledged scope that still refuses them.
  Until then, preflight refuses and this ADR promises nothing.
- **Windows story stays open under row 36.** This ADR chooses none of (a) a
  narrow audited `unsafe` exception, (b) the safe high-level `windows` crate
  (new dependency, own ADR), or (c) accepting Linux/macOS-only with loud
  docs. `docs/DEFERRED.md` row 36 is referenced and intact.
- **Registration-time runtime observation can go stale.** A runtime
  installed mid-session is invisible to describe until re-registration; the
  policy system probe's `refresh()` is the existing remedy on the
  enforcement path. Refreshing describe facts mid-run is future work, not a
  correctness gap (the report never gates anything).
- **macOS spawn-time differences are not probe-time differences.** The
  Docker-Desktop VM, daemon health, and image availability surface at
  invocation, fail-closed, on both POSIX platforms; the table reports the
  probe only.
- **The routing marker stays a producer assertion** (ADR-72 §2 residual,
  restated by reference, not re-decided): no independent proof of
  confinement exists beyond the audited argv.
- **`ReadOnlyFs` / `NetworkIsolated` still need their own ADRs** (ADR-72 §1
  residual, unchanged).

---

*Accepted 2026-10-12: H10 sandbox-backend decision. Capability interface
shipped (`425eaef`, `bcbedca` plus the managed-process carry-forward
`a378735`); no checkpoint backend ships with it.*
