# ADR-49: Config-first model catalog — providers as data

**Status:** Accepted (2026-08-08) — implemented across Phase 2 M3
    (`be43294`, config-first model catalog + tier-1 default-model fallback)
    and Phase 4 (`8436462` `concerto health`, `6eec62b` env-fallback fix +
    config-only endpoint docs).
**Date:** 2026-08-08
**Deciders:** Concerto architecture
**Supersedes:** Phase 4 of the provider-first redesign plan
    (`docs/ARCHITECTURE-V2.md`) — the config-only endpoint / providers-as-data
    items this ADR finalizes.
**Composes with:** ADR-42/ADR-45 (fallback ladder) — this ADR's resolved-model
    precedence is the tier-1/tier-1b pin source; ADR-46 (reasoning echo knob)
    and ADR-48 (cache breakpoints) — both knobs live on `ProviderConfig`.

## Context

The V2 redesign's "everything is config data" principle (§1.3, §5) demands
that any provider/model/role/endpoint be expressible in config without
recompiling: point one `api_base` at an OpenAI-compatible gateway and serve
several models off it. Today the provider table in
`crates/config/src/schema.rs` carries the model catalog data, but the
resolution of "which provider serves model M" and "which model is the
default" was spread across call sites. The redesign (Phase 4) prescribes a
small, additive surface: **additional models on a provider config**, a
**resolved default model** shared by orchestrator and CLI, and a **key
resolution**
that both the factory and `concerto health` agree on.

Facts established by light inspection of the current code
(`crates/config/src/schema.rs`):

- `ProviderConfig` already carries `model` (legacy), `cached_models`
  (auto-discovery cache), and gained **`extra_models: Vec<String>`** —
  models this provider advertises for resolution. The rule: they are
  offering candidates, never shadow the primary `model`
  (`ProviderFactory::config_for_model`).
- `ProviderConfig.reasoning_echo: Option<String>` — `"always"` /
  `"if-present"`
  echo policy for OpenAI-compatible providers (ADR-46).
- `ProviderConfig.cache_breakpoints: bool` — Anthropic prompt-cache
  `cache_control` breakpoints (ADR-48 decision 3 — the opt-in extension
  point); no-op for non-Anthropic families; off preserves current wire.
- `ProviderConfig.effective_api_key(store)`: **keyring first**, then the
  `<PROVIDER>_API_KEY` env var (provider uppercased) — the exact resolution
  `ProviderFactory::build` uses; when both are missing the original keyring
  error is returned.
- `ModelSettings.resolved_default_model(multi_agent)`:
  **`multi_agent.default_model` wins when set**, otherwise
  **`self.global_default_model`** fills the tier-1 target; whitespace-only
  values are treated as unset; `None` disables the tier.
- `MultiAgentConfig.default_model_fallback` (default **true**): tier 1b of
  the ladder re-dispatches the role on the run's default provider serving the
  global default model (ADR-45).

## Decision

Adopt a **config-first model catalog**: providers and their model offerings
are configured data, and the "which model runs what" resolution is computed
against the config (with the model-merit list as data, not hard-coded).

1. **Provider model lists as data.** A `ProviderConfig` describes every
   model it can serve: `model` (primary), `cached_models`
   (discovery stamp), and `extra_models` (user-declared extras for a
   single-`api_base` gateway). `ProviderFactory::config_for_model`
   treats each entry as an offering candidate; `extra_models` never shadow the
   primary route. This realizes the "point config at a brand-new
   OpenAI-compatible endpoint via config only" exit gate.
2. **Key resolution contract.** The runtime uses
   `effective_api_key`: **keyring → `<PROVIDER>_API_KEY` env**. The env
   fallback is part of the resolution contract, not a CLI-only convenience.
3. **Default-model precedence.** `resolved_default_model(multi_agent)` =
   **`multi_agent.default_model` > `global_default_model`**,
   whitespace-normalized, `None` when neither. Used by both the orchestrator
   and `concerto health` so they agree on the tier-1 pin.
4. **Tier-1 fallback onto the global default.** The fallback ladder's tier 1
   re-dispatches the same role on the resolved default model when the binding
   pipe offers it; tier 1b (ADR-45) rebuilds the role on the run's default
   provider when the bound provider is the failure
   (`MultiAgentConfig.default_model_fallback`).
5. **Reasoning-echo / cache-breakpoint knobs as data.** The ADR-46 echo
   policy and the ADR-48 cache breakpoint are config fields on the provider,
   serde-default, forward-compatible — no provider-ID hard-coding.

### Explicitly deferred (do NOT mark complete)

The **flat schema flattening** — merging
`ProviderConfig` / `ModelPinConfig` / `MultiAgentConfig` into a single
catalog schema (`docs/ARCHITECTURE-V2.md` §8:
"Full flattening of `ProviderConfig`/`ModelPinConfig`/
`MultiAgentConfig` into one catalog schema remains deferred"). This ADR
ratifies the catalog *orientation* (providers, their models, the resolution
precedence, and the knobs as data on the existing tables) — it does **not**
collapse the three config tables into one. That flattening plus the deferred
feature-config surface (per-role fallback disable, per-tier retry/backoff
counts, ladder-locked tier targeting) remains a separate decision, recorded
in §7 of the plan and the deferred-feature-config style in
`docs/adrs/ADR-52-orchestration-safety-gates.md` — not ratified here.

## Consequences

- **Positive.** A user can point config at a brand-new OpenAI-compatible
  endpoint and get a catalog-visible model without recompiling; `concerto
  health` surfaces the resolved provider/model/key and shares the same
  precedence logic as the runtime; additive serde-default keeps existing
  configs loading unchanged; reasoning echo and cache breakpoints are
  per-provider knobs instead of provider-ID switches.
- **Negative.** The three config tables stay distinct, so "the catalog" is a
  virtual concept over ProviderConfig/ModelPinConfig/MultiAgentConfig rather
  than one schema — that flattening is still open. Non-`OpenAI` provider
  families treat `extra_models`/`cache_breakpoints` as no-ops, so a few
  fields are dormant for most providers.
- **Risks.** Two resolution functions (`resolved_default_model` + the
  factory/key) could drift from each other — they are kept in lockstep by
  sharing the same `effective_api_key` / config schema paths and by `concerto
  health` asserting the resolved stack matches runtime expectations.
- **Migration.** Purely additive: `extra_models`,
  `reasoning_echo`, `cache_breakpoints` are `#[serde(default)]`; legacy `model`
  still works; no config tables or persistence rows are rewritten.

## Review notes

- ADR-46 / ADR-48 are consumed here as *knobs* (reasoning echo, cache
  breakpoints) — their decisions are unchanged.
- ADR-42/45 feed this ADR's precedence rule: `resolved_default_model` is the
  tier-1 pin source; `default_model_fallback` is tier 1b (ADR-45). This ADR
  does not re-decide the ladder.