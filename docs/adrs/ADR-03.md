# ADR-03: Configuration System — `figment`

**Status:** Accepted
**Date:** 2025-07-11
**Deciders:** Concerto architecture

## Context

Configuration needs to be layered: hardcoded defaults, then an optional
`~/.config/concerto/config.toml`, then environment variable overrides
(`CONCERTO_*`) — the env layer matters specifically for CI, where
`CredentialStore::from_env()` and other test-mode behavior must be reachable
without touching a real config file or the OS keychain. The config schema
also needs versioning from day one (Guiding Principle 10: schema migrations
are not optional) so a `schema_version` mismatch on load fails clearly
instead of silently misreading a field.

## Decision

Use `figment` with the `Toml` and `Env` providers, merged in order: hardcoded
`Default` → `config.toml` (if present) → `CONCERTO_*` env vars (highest
precedence). `AppConfig::schema_version` is checked against a crate constant
on every load.

## Consequences

- Adding a new config source later (e.g. CLI-flag overrides in Phase 3) is
  one more `.merge()` call, not a rewrite.
- Schema mismatches fail fast with a clear `ConfigError::SchemaMismatch`
  rather than a confusing deserialization error or, worse, a silently wrong
  default.
- `figment`'s error messages are reasonably good but still require some
  translation into our own `ConfigError` variants at the boundary — this
  happens once, in `config::load_config`, not at every call site.

## Alternatives Considered

- **`config` crate:** similar layering model, more established, but
  `figment`'s `Serialized::defaults()` provider made expressing "defaults are
  just a `Default`-impl'd struct" more direct for this codebase's style.
- **Hand-rolled layering (read file, then overlay env vars manually):**
  no dependency, but reinvents a solved problem and is exactly the kind of
  thing Guiding Principle 1 ("leverage, don't reinvent") argues against.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
