# ADR-32: Explicit provider failures and safe interactive policy defaults

**Status:** Accepted
**Date:** 2026-07-20
**Deciders:** Concerto architecture

## Context

Production provider construction silently substituted `MockProvider` when a
provider was absent, unsupported, or missing credentials. A user could receive
plausible canned output that looked like a successful model response. Separately,
an absent policy had alternated between denying every tool and approving every
tool, making a fresh install either unusable or unnecessarily ungated.

The desktop approval dialog also represented policy actions as plugin grants.
It did not distinguish one operation from a session grant, used names that did
not match the actual `filesystem`, `shell`, and `git` tools, and did not show a
useful action summary.

## Decision

- Production provider construction returns typed errors for missing
  configuration, missing credentials, and unsupported provider types.
- Mock providers remain test doubles only. No production resolution or factory
  path may select one implicitly or explicitly as a fallback.
- An absent or empty policy activates the centralized safe default preset:
  project file reads/listing/existence checks are allowed; filesystem mutation,
  shell execution, Git operations, and unmatched tools require approval; known
  destructive shell patterns are denied.
- Custom policy rules replace the preset, retain first-match ordering, and
  default-deny unmatched actions.
- Desktop approvals offer `Allow once`, `Allow this tool for the session`, and
  `Deny`, with the exact project path, command, or operation displayed.
- CLI production execution uses the same policy preset and an interactive
  approval sink. Explicit evaluation harnesses may opt into the named
  permissive preset.

## Consequences

- Provider setup failures are visible and cannot masquerade as model output.
- Fresh installs can inspect projects without prompt fatigue while retaining a
  clear authorization boundary for mutations and commands.
- Session grants are temporary and accurately labelled; persistent policy
  changes remain deliberate Settings actions.
- Tests may continue using mock provider types without making them reachable
  from production configuration.
