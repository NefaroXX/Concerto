# ADR-31: Model-first selection with internal provider routing

**Status:** Accepted
**Date:** 2026-07-20
**Deciders:** Concerto architecture
**Supersedes:** ADR-24 session-default routing
## Context

ADR-24 correctly required a provider configuration and model to remain paired
through execution, but exposed that implementation pair as two separate user
choices and persisted a global default provider. Provider records are transport
details: credentials, endpoint, timeout, discovery cache, and protocol type.
Users intend to choose models. Requiring both choices duplicated controls across
Settings, Chat, the quick panel, and Studio, and allowed them to drift.

Multi-agent roles already persist the provider configuration ID beside their
model. The UI therefore does not need a separate provider selector or provider
default to preserve deterministic execution.

## Decision

- User-facing execution selectors display models only.
- Provider configurations remain in Settings solely to manage credentials,
  endpoints, discovery, and protocol records.
- Selecting a model resolves a ready provider configuration internally and
  stores that route with the selection or role assignment.
- When the existing assignment's provider offers the selected model, Concerto
  preserves that route. Otherwise it chooses the first ready configured
  provider advertising the model, in stable configuration order.
- There is no persisted global/default provider and no first-provider execution
  fallback.
- Single-agent chat stores a default chat model and resolves its provider route
  before dispatch.
- Every role participating in a multi-agent run requires an explicit model
  assignment with an internally stored provider route. Missing or stale routes
  stop dispatch with a visible setup error.
- Runtime events and audit records may include the resolved provider ID for
  diagnosis, but ordinary selectors and status summaries present the model.

## Consequences

- Chat, Quick Panel, Settings, and Studio cannot disagree because they edit the
  same model intent.
- Provider/model execution remains deterministic without making provider
  transport a routine user decision.
- Two ready providers can advertise the same model. Existing assignments retain
  their route; a new selection uses stable configuration order.
- Removing a provider invalidates only assignments routed through it and never
  silently redirects them to a default provider.
