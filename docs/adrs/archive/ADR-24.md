# ADR-24: Deterministic Provider/Model Routing

> **Archived** — superseded by [ADR-31](../ADR-31.md) (consolidated 2026-08-22).
> See [docs/adrs/README.md](../README.md) for the current index. Retained
> verbatim as the historical record of the capability-tier removal; not active
> guidance. ADR-31 refines this decision into model-first selection.

**Status:** Superseded by ADR-31

## Context

Capability tiers assigned subjective numeric strength to whole providers and
then used those values to choose models for agent roles. Provider-wide tiers
were not reliable model metadata, made valid explicit selections fail, and
allowed the routing engine to select a model independently from the provider
instance that would execute the request.

That separation caused multi-agent mode to behave differently from the Chat
selection and could dispatch a model through the wrong provider configuration.

## Decision

Concerto does not use capability tiers.

Provider configuration ID and model name form one routing identity and remain
paired from UI selection through provider construction and request dispatch.

For a multi-agent run:

1. The provider/model selected in Chat is the session default for every role.
2. A role assignment explicitly overrides that pair for its role.
3. The coordinator and specialist providers are constructed with their resolved
   role models; the planner uses the resolved Coordinator pair.
4. Missing provider IDs or empty models stop dispatch with a visible,
   user-facing configuration error.
5. Objective model compatibility metadata, such as tool-calling support and
   context window, may block an incompatible request. It does not rank models.
6. Spend limits validate explicit assignments. Cost-based fallback is available
   only when no session or role assignment exists.

## Consequences

- Multi-agent mode honors the same visible selection as single-agent mode unless
  the user has deliberately configured a role override.
- Routing is deterministic and explainable without subjective model rankings.
- Discovery data and provider credentials remain associated with stable provider
  configuration IDs.
- Existing serialized `capability_tier` values are ignored during migration.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](../README.md)).*
