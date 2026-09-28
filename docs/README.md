# `docs/` index

Start here. This file is the map; nothing below is a substitute for it.

## Where to look

| If you want to know… | Read |
|---|---|
| What exists right now, and what is still unverified in real use | [STATUS.md](STATUS.md) |
| What is outstanding, and what condition would reopen it | [DEFERRED.md](DEFERRED.md) — the single outstanding-work register |
| How the system fits together | [architecture.md](architecture.md), [crate-graph.md](crate-graph.md) |
| Why a decision was made | [adrs/README.md](adrs/README.md) — index; superseded ADRs live in [adrs/archive/](adrs/archive/) |
| How a provider/model is configured | [models.md](models.md), [missing-providers.md](missing-providers.md) |
| How tools are allowed or denied | [policy-rules.md](policy-rules.md), [security-threat-model.md](security-threat-model.md) §6 |
| How multi-agent roles and relationships work | [agent-collaboration.md](agent-collaboration.md) |
| How skills and MCP servers work | [skills.md](skills.md), [mcp.md](mcp.md) |
| Which shells are supported | [shell-profiles.md](shell-profiles.md) |
| How to configure the app | [config.toml.example](config.toml.example), [api/openapi.json](api/openapi.json) |
| How to test a change for real | [live-test-template.md](live-test-template.md) (the form to copy) and [`../TESTING.md`](../TESTING.md) (the acceptance bar) |
| Unstarted work with no deferral decision | [TODO.md](TODO.md) — now a short list, by design |

## Live documents

Describe current behaviour or a still-active decision. Edit these when the
code changes.

- [STATUS.md](STATUS.md) · [DEFERRED.md](DEFERRED.md) · [TODO.md](TODO.md)
- [architecture.md](architecture.md) · [crate-graph.md](crate-graph.md)
- [agent-collaboration.md](agent-collaboration.md) ·
  [models.md](models.md) · [missing-providers.md](missing-providers.md) ·
  [policy-rules.md](policy-rules.md) · [security-threat-model.md](security-threat-model.md)
- [skills.md](skills.md) · [mcp.md](mcp.md) · [shell-profiles.md](shell-profiles.md) ·
  [desktop-cli-parity.md](desktop-cli-parity.md)
- [live-test-template.md](live-test-template.md) (blank form) and the two
  worked examples with recorded results:
  [live-test-skills-mcp.md](live-test-skills-mcp.md),
  [live-test-multi-agent-eval.md](live-test-multi-agent-eval.md)
- [config.toml.example](config.toml.example) · [api/openapi.json](api/openapi.json)
- [adrs/](adrs/README.md) — decisions, each with a status line

## Plans and design drafts

Accepted designs or referenced design docs, each carrying its own status line.
These are **not** registers — for what is outstanding, use `DEFERRED.md`.

- [custom-ai-shell-plan.md](custom-ai-shell-plan.md) — AI-native shell phases
  A–F; status: A and B implemented, C–F planned (deferred item: `DEFERRED.md`
  row 24)
- [hybrid-ui-plan.md](hybrid-ui-plan.md) — hybrid UI tiers; status: minimal
  (PR #49) and medium (PR #97) merged, full scope pending (deferred item: row
  37)
- [orchestration-runtime-bridge-plan.md](orchestration-runtime-bridge-plan.md) —
  ADR-58 P2+P3 runtime–blueprint bridge; status: **draft**, pre-implementation
  design, no code changes yet
- [proxy-tool-call-fix.md](proxy-tool-call-fix.md) — shipped parsing fixes plus
  the standing caveat that parsing fixes do not prove proxy support
- [design/](design/) — reference design docs, not ADRs:
  [outstanding-work.md](design/outstanding-work.md) (score accordion V2 +
  signature text motion, 2026-09-18) and
  [text-presentation-animation.md](design/text-presentation-animation.md)
  (text presentation and animation guidance, 2026-09-03)

## Research (`docs/research/`)

Background, prior art, and analysis. Nothing here is a commitment or a status
claim; do not cite it as evidence that something exists.

- [ARCHITECTURE-V2.md](research/ARCHITECTURE-V2.md) — 2026-08-07
  provider-first draft; the shipped parts became ADRs 46–53
- [codebase-world-class-plan.md](research/codebase-world-class-plan.md) —
  aspirational engineering estimate
- [skills-mcp-extensions-plan.md](research/skills-mcp-extensions-plan.md) — the
  plan ADR-43 accepted; shipped as [skills.md](skills.md) + [mcp.md](mcp.md)
- [orchestration-blueprint.md](research/orchestration-blueprint.md) ·
  [concurrent-agent-runtime.md](research/concurrent-agent-runtime.md) ·
  [multi-provider-resilience.md](research/multi-provider-resilience.md) ·
  `ai-native-shell-*.md` (research brief, implementation plan, expanded reference)

## Historical (`docs/archive/`)

Kept for provenance. **No live authority** — do not follow these for current
behaviour, and do not cite them as evidence a feature exists.

- [archive/STUB-FINDINGS.md](archive/STUB-FINDINGS.md) — stub audit; all 8 stubs
  resolved. `STUB-FINDINGS.md` in this directory is a pointer to it
- [archive/batch-3-studio-ux.md](archive/batch-3-studio-ux.md) — Studio UX spec
  superseded by the ADR-59 revision of 2026-08-15
- [archive/handoff-2026-08-14.md](archive/handoff-2026-08-14.md) — point-in-time
  session handoff
- [archive/provider-model-ux-implementation-notes.md](archive/provider-model-ux-implementation-notes.md)

## Rules for this directory

1. A document is **live**, a **plan**, **research**, or **historical** — pick
   one and say so at the top. Nothing sits in `docs/` root without a status
   line.
2. Deferred work is registered in [DEFERRED.md](DEFERRED.md) with a source, a
   re-entry condition, and a size. It does not live in a plan, an ADR, or
   `TODO.md`.
3. Do not delete a decision or a recorded result. Supersede, archive, or point
   at the replacement — and keep the old path resolving if anything links it.
4. Never leave a dangling path. When a file moves, update its references,
   including doc comments in source.
5. Every claim carries a checkable reference: a file, a line, a test name, or a
   commit SHA.
