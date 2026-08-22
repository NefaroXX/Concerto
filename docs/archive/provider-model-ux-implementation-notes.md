# Provider / Model-UX Rework — Implementation Notes (Phase 0)

> **Status:** Historical implementation record. For current user behavior and
> configuration, see [`models.md`](models.md). Facts below describe the rework
> checkpoints and should not be treated as the current public contract.

Companion to a historical provider/model rework plan that is not retained in
this repository. Captures the source-verification gate results and decisions
that drove the later phases.
Verified against the working tree at the start of the `feat/provider-model-ux-rework`
branch.

## Decisions confirmed by Sol
- **Config save policy:** immediate atomic persistence for provider/model intent.
  The desktop Settings view already persists everything on `SaveSettings`; we
  additionally persist lightweight selections (chat provider/model, discovery
  cache metadata) immediately, as the app already does for `SetActiveProvider`.
- **Quick panel:** the collapsible adjacent right-side panel (plan §11) is
  approved. Built in Phase 7 from shared state only.

## Confirmed facts (source of truth)
- `ProviderConfig` (crates/config/src/schema.rs): `id` (`#[serde(default)]`, may
  be empty on legacy entries), `name`, `provider`, `model`, `api_base`,
  `timeout_seconds`, `keyring_key`. `id` is the stable key used everywhere.
- ID generation convention: `format!("prov_{}", concerto_core::ids::Ulid::new())`
  (desktop settings) and fallback `format!("prov_{}", pc.provider)` (factory).
- Persistence: `concerto_config::save_config` writes TOML atomically. No staging
  in the desktop view; Settings saves on `SaveSettings`. Chat provider/model
  changes persist via `App::persist_active_model_selection`.
- `CredentialStore` (crates/config/src/credentials.rs): `get` returns
  `Result<String, ConfigError>`; missing key => `Err(CredentialMissing)`.
  `exists` => `get(...).is_ok()`. Test mode via `CredentialStore::from_env()`.
- Provider capability split (crates/providers/src/lib.rs `list_models_for_provider_async`
  + factory.rs `build`): anthropic/openai/google/openrouter/nim/opencode require
  a key and support discovery; ollama requires no key and supports discovery.
  Capability metadata was previously hardcoded in two places — now centralized in
  `crates/providers/src/provider_defs.rs::provider_definition`.
- `ModelRegistry` (model_registry.rs) exists but was not wired into provider
  resolution / chat UI. The plan's "single source of truth" is satisfied by
  `provider_defs` (definitions + resolver + readiness), not by repurposing
  `ModelRegistry`.
- Cache/state: `crates/config/src/legacy.rs::data_dir()` (`~/.local/share/concerto`)
  is the right home for the discovered-model cache (Phase 3). No separate cache
  dir currently exists; reusing `data_dir` is acceptable.
- Delayed messages: `iced::Task::perform(async { sleep(...).await; }, cb)` is the
  existing convention (app.rs screenshot-status clear). Phase 5 reuse it with a
  generation token.
- Verified project checks:
  `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test -p concerto-config -p concerto-providers -p concerto-desktop`.
  CI jobs (GitHub Actions): fmt, clippy, test, deny. Toolchain pinned to
  1.96.0; workspace MSRV 1.88.

## New code introduced in Phase 1
- `crates/providers/src/provider_defs.rs`:
  - `CredentialRequirement`, `ModelDiscoverySupport`, `ProviderDefinition`.
  - `provider_definition(type) -> ProviderDefinition` (single source of truth).
  - `PROVIDER_TYPE_IDS` (UI display order).
  - `ProviderModelCache` (+ `normalize`, `fingerprint`) for Phase 3.
  - `model_options_for(provider, def, cache)` — shared, deterministic resolver.
  - `ProviderReadiness` + `provider_readiness(...)` — pure validation.
- `crates/config/src/schema.rs`: `ProviderConfig::ensure_id`,
  `ModelSettings::repair_ids` (idempotent empty/duplicate repair).
- `crates/config/src/lib.rs::load_config`: calls `repair_ids` on load so legacy
  configs are fixed in-memory; persisted on next save.

## Risks / gaps to watch in later phases
- Desktop crate requires system deps (protoc, sqlite3, clang, nettle,
  X11/Wayland + GL/Vulkan) to compile; full `-p concerto-desktop` verification
  must happen in an environment with those installed.
- Known-models catalog in `provider_defs` is intentionally tiny and hand-maintained;
  do not expand it with speculative IDs — discovery + custom entry cover the rest.
- `ProviderFactory::build_all` still uses `prov_{provider}` fallback for empty ids;
  with `repair_ids` on load this path is now only a safety net.
