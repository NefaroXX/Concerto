# ADR-57: Config change propagation without restart

**Status:** Accepted (2026-08-13) — implemented on `dev` at `0f01fa7`
    (desktop watcher + reconcile helper) and `99b6a89` (CLI per-run reload);
    ADR landed at `104561f`. All gates green: desktop lib 326/326, CLI
    140/140, fmt + clippy `-D warnings` clean on both crates.
**Date:** 2026-08-13
**Deciders:** Concerto architecture
**Supersedes:** Nothing — new capability; no settled ADR is contradicted.
**Composes with:** ADR-44 (`project_roots` consent — the reloaded
    `effective_roots` is a **union** of config roots with runtime-consented
    roots, so an external edit never revokes consent); ADR-49
    (config-first catalog, providers as data — external provider/model edits
    must flow into the next run); ADR-55/56 (intent routing —
    `classifier_enabled` edits flow through the reloaded config clone into
    the next run; no special handling needed); ADR-43 (desktop Settings
    sections are config-driven with next-run semantics — generalized here
    into a watcher).

## Context

Config changes today are **restart-only**. Both surfaces load config once at
startup and then run against a frozen snapshot:

- **Desktop** (`crates/desktop/src/app.rs`): `App` fields are derived from
  config at startup, `app.rs` ~638–777 (line anchors at time of writing —
  `multi_agent` 681, `session_cap` 686, Settings-view state 695 from
  `global_config`, terminal 700 from `resolved_shell_settings`,
  `active_provider_id`/`active_model` 772, `chat_model_options` 775, memory
  enabled 776, orchestration-studio 764, `effective_roots` 675). The submit
  path (~2050–2196) clones `self.config` into a `ServicesBuilder`
  (~2074–2174) but freezes `active_provider_id`/`active_model`
  (2090–2097), `multi_agent` (2098), `fast` (2099), and `project_dir`
  (2100) into locals — so a reloaded config only **matters** if those `App`
  fields are re-derived first.
- **CLI** (`crates/cli/src/app.rs`): config is frozen at `configure()`,
  ~210–234; `dispatch_message` 518–589 freezes `selected_model` (540),
  `multi_agent` (578), `fast`, and `project_dir`; `ServicesBuilder` gets a
  config clone (582).

An external edit of `config.toml` or `.concerto.toml` therefore does nothing
until restart — and the next settings save overwrites it. Layer facts
(`crates/config/src/lib.rs`, at time of writing):
`AppConfig::default` → global file `~/.config/concerto/config.toml` (legacy
fallback `~/.config/opencode-rs/config.toml`; `config_path()` returns
existing-new else existing-old else new) → `{project}/.concerto.toml`
(legacy `.opencode-rs.toml`) → `CONCERTO_*` env. `load_config(global_path,
project_root)`; `load_global_config` excludes project+env (the settings
editor uses it); `save_config` is an in-place truncate-write
(`lib.rs:222–232`). The config crate is deliberately **sync-only** (no
tokio); this ADR preserves that.

Every desktop write path builds from `self.global_config` (settings save
1259–1261, model persist 2238–2243, studio persist 2304–2317, toggle 1088)
— so a reload must refresh **both** `self.global_config` **and**
`self.config`, or the next save silently overwrites the external edit.

Precedents this ADR reuses:

- **Memory watcher** (`crates/memory/src/watcher.rs`): `notify = 7` +
  `notify-debouncer-mini` 0.5 with a 1s debounce; the debouncer's callback
  runs on its own std thread and must not block (try_send only); a tokio
  `mpsc::channel(100)`; `recv()` is `tokio::select!` over
  `concerto_core::CancellationToken` + the receiver; the `Debouncer` is kept
  alive by the owning struct; tests use tempdir + tokio timeout.
- **CLI event loop**: synchronous crossterm polling (`event::poll` 100ms),
  no subscription system — hence per-run reload instead of a watcher.

## Decision

External edits of config files take effect on the **next agent run without
restarting** — via a filesystem watcher in the desktop app and a per-run
reload in the CLI TUI. Seven sub-decisions:

### 1. Placement — desktop watcher, CLI per-run reload, config crate untouched

Desktop gains a local watcher module `crates/desktop/src/config_watch.rs`
mirroring the memory-crate watcher shape. The config crate stays sync-only —
no tokio/notify dependencies are added to it. The CLI gets **no watcher**:
it reloads at the top of `dispatch_message` on every dispatch.

### 2. Watch targets — config + project dirs, exact-name filtered

`create_dir_all` the config dir at watcher setup. Watch **both** parent
dirs non-recursively (new + legacy global config dirs) **plus** the current
project dir. Filter events by exact file name (`config.toml`, the legacy
global name, `.concerto.toml`, the legacy project name). This covers
in-place write, atomic rename, and file creation. One debouncer, multiple
`watch()` calls. Event noise from unrelated files sharing the config dir is
neutralized by the name filter.

### 3. Message + single reconcile helper

New `Message::ConfigReloaded` (the `Message` enum derives `Clone`,
`app.rs` ~101–219). One shared helper, `reconcile_config_from_reload()`:

- **(a)** Reloads `load_global_config(None)` → `self.global_config` **and**
  `load_config(None, Some(project_dir))` → `self.config`.
- **(b)** **Mandatory equality short-circuit:** if the reloaded `AppConfig`
  equals the current one (`AppConfig: PartialEq`), skip all re-derivation.
  This makes self-induced events (a settings save rewrites the exact file
  the watcher is watching) **provably no-ops**, and prevents flip-back when
  `.concerto.toml` overrides a `default_enabled` key.
- **(c)** On parse/schema error: **keep last-good config**, log, toast once
  per broken period (repeat toasts suppressed while the file stays broken),
  retry on the next event — no poll-retry.
- **(d)** On success, re-derive **in order**: run-mode flags (`multi_agent`
  — the file is truth); `active_provider_id`/`active_model` via
  `configured_default_route`; `chat_model_options`; memory config;
  session-cap chip; terminal profiles (`resolved_shell_settings`);
  orchestration-studio model sync — **cache only, never
  `load_from_config`** (protects unsaved studio edits); Settings-view cached
  provider labels/ids refresh; `effective_roots` refresh as a **union** of
  config roots with runtime-consented roots (ADR-44 — outside-root consent
  must not be revoked by an external edit).
- **(e)** Never writes config — reload is **read-only**, so there is no
  feedback loop.

#### Destructive-gating (3a)

Memory lifecycle teardown (`sync_memory_configuration` when the file
disables memory) is deferred until `run_status == Idle` — an active run
holds memory-store clones plus the reindex watcher wired to
`ActiveMemoryServices.cancel`. **Memory parameter changes**
(ttl/embedding/search paths) are **not** hot-applied (re-init would drop the
vector cache) — documented as restart-scoped. Similarly, no mid-run reset of
`session_manager` or the VFS.

#### Scope exclusions (3b)

No mid-session `project_dir` re-rooting; the active session is untouched;
Settings form state is never rebuilt by the watcher (protects unsaved
edits); theme/prefs JSON (`UserPrefsStore`) is out of scope for v1;
env-var changes already apply live per call (unchanged); API-server
env-only config is unchanged; `session_manager` `git_auto_init` startup
capture is unchanged.

### 4. Consolidation of existing reload sites

The existing reload sites — settings save (1267–1281), multi-agent toggle
(1087–1104), provider/model persist (2249–2257), Studio save (~2324) —
collapse onto `reconcile_config_from_reload()` where semantics match.
Project re-select (1751–1779) keeps its extra session-reload logic but also
**re-arms the watcher path set** (a project-dir change changes what is
watched).

### 5. CLI — per-run reload with remembered flags

Per-run reload at `dispatch_message` start (before ~line 522) refreshing
`global_config` + `config`; the equality short-circuit per dispatch skips
no-ops. CLI flags (`-m`/`--multi-agent`, `-f`) are **remembered at startup
and re-applied after every reload**, so a config edit cannot clobber an
explicit flag. The in-app model cycler persists to the file immediately, so
the file-as-truth rule applies to it too.

### 6. Semantics — the file is truth

UI flows that override config (multi-agent toggle, chat-header model pick)
persist to the file **before** use, so watcher events from self-writes are
no-ops via the equality short-circuit. Residual external-edit-vs-UI-pick
races are last-writer-wins and converge under the 1s debounce — accepted
and documented.

### 7. Subscription identity

The watcher stream needs a **stable identity**: `subscription()`
(`app.rs` 3255, `Subscription::batch` of 9 streams at time of writing) runs
every frame, and a fresh identity would recreate the debouncer each frame,
resetting the 1s window. Use `Subscription::run_with` with a persistent
handle.

### In-scope / out-of-scope (v1)

| Scope | Items |
| --- | --- |
| **In** | global `config.toml` external edits → next run; `.concerto.toml` creation/edits → next run; run-mode re-derive; settings-save-induced no-op events |
| **Out** | theme/prefs JSON, env vars, API server, memory params, DPI/theme, keyring (already per-call), plugin `.wasm` files on disk (unchanged mechanism) |

## Consequences

- **Positive.** No-restart workflow for config edits in both surfaces; one
  `reconcile_config_from_reload()` helper removes **five duplicated reload
  sites** (a bug class eliminated: divergent re-derivation logic); self-induced
  events are deterministic no-ops; sessions always run on last-good config,
  even during a broken edit window, and recover on the next event with no
  restart.
- **Negative / costs.** Desktop gains `notify` + `notify-debouncer-mini`
  dependencies (mirroring the memory crate); ~150 lines of watcher plus
  ~100 lines of reconcile helper; the watcher is platform behavior
  (inotify/FSEvents/Windows) — covered by the 1s debounce and exact-name
  filtering; the CLI per-run reload costs one small TOML parse per dispatch
  (negligible).

## Testing

- **Watcher unit tests** (tempdir): write → signal received; cancel →
  `recv` returns `None`; malformed file → error path. Keep-last-good
  semantics live in the handler, tested via helper decomposition where
  feasible.
- **Reconciler tests** (desktop): equality short-circuit is a no-op;
  re-derive fields change on a differing config; memory teardown is gated
  on `Idle`.
- **CLI**: per-run reload is applied; flag precedence is preserved (a config
  edit cannot clobber `-m`/`-f`).
- **Manual checklist**: run the desktop, edit `config.toml` externally, the
  next run uses the new values; break the file → toast + last-good; fix the
  file → the next edit reloads; a settings save does not flip run-mode; no
  restart involved.

## Review notes

- D3b's equality short-circuit is the load-bearing detail: without it, a
  settings save would re-trigger the watcher, and retained-UI-overrides
  (toggle, model pick) would flip back against a `.concerto.toml`
  `default_enabled`. `AppConfig: PartialEq` is the cheap invariant that makes
  self-induced events provably inert.
- D3a keeps the watcher user-visible read-only: it may disable memory, but
  only between runs — never under one. D7 is a correctness-of-observability
  decision, not an optimization: the debounce window is what makes
  last-writer-wins converge (D6).