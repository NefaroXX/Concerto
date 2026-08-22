# ADR-44: Project-root confinement and consent gating

**Status:** Accepted (2026-08-05)
**Date:** 2026-08-05
**Deciders:** Concerto architecture

## Context

A security audit found three gaps (#104, #105, #106). Gap #106: `validate_project_dir` in crates/api-server/src/routes.rs only checks that the supplied path exists and is a directory — there is no allowlist or root confinement — so `POST /v1/sessions` lets any caller holding the single shared `CONCERTO_API_KEY` root a session at any filesystem location the server process can read/write (`/etc`, another user's home, etc.). Every filesystem/shell tool the orchestrated agent runs then operates inside that chosen root: the tool sandbox (`VirtualFs`, `resolve_path`) confines operations to the session root, but the root itself is caller-chosen with no consent and no restriction.

- The api-server explicitly supports non-loopback binding (main.rs gates it on `CONCERTO_API_KEY`), so this is a real remote hole.
- The desktop app (the only interactive client) does NOT call the api-server — it creates sessions via the library directly — so a GUI consent dialog cannot gate HTTP-originated sessions. The HTTP server is headless: there is no consent channel.
- No in-workspace crate consumes concerto-api-server today; it is an optional standalone surface for programmatic/remote use.

## Decision

Introduce a project-root allowlist with hard server-side enforcement and a local consent gate. The allowlist is a new shared config concept; the server refuses out-of-root session roots outright; a non-loopback bind additionally requires the allowlist to be non-empty; the desktop shows a consent modal for out-of-root project opens when the allowlist is configured.

### 1. `project_roots` config (concerto-config)

New shared config concept: `project_roots: Vec<Utf8PathBuf>` in `AppConfig` (concerto-config), default empty. Sources: config file plus the `CONCERTO_PROJECT_ROOTS` env var (path-separated; parsed in the config crate so every consumer sees the same merged value; env wins per the existing figment layering).

### 2. Hard server enforcement (`validate_project_dir`)

`validate_project_dir` rejects canonicalized paths outside the configured roots with 403. Canonicalization (via `std::fs::canonicalize`) resolves `..` and symlinks before comparison, so traversal attempts cannot escape. No bypass flag: the server has no consent channel, so out-of-root is simply refused.

### 3. Startup gate (non-loopback binding)

Binding to a non-loopback address now requires BOTH `CONCERTO_API_KEY` and non-empty `CONCERTO_PROJECT_ROOTS` (extends the existing gate in main.rs). With roots unset: behavior is permissive on loopback (unchanged local behavior, no breakage) — the restrictive defaults only apply when the server is exposed.

### 4. Desktop consent gate (local flows only)

When `project_roots` is configured and the user opens/switches to a project dir whose canonical path is outside the roots, the desktop shows a permission-gate modal (composed in the existing system-dialog stack, palette colors): **[Allow]** proceeds for the process lifetime (adds the root to the effective allowlist) and **[Deny]** aborts the switch. When roots are unset, no gate (consistent with the loopback-permissive default). This gate is consent/awareness for local use — it cannot and does not protect the api-server, which is enforced by §2 and §3.

### 5. Deferred (noted for permanence)

A server-side "pending-approval" consent flow for out-of-root HTTP sessions: the session would be created in a pending state, an event emitted over the bus/SSE, and an interactive client approves/rejects. Deferred because there is no in-tree interactive HTTP client today; revisit if one materializes. Also deferred: persistent "always allow" for the desktop gate (Allow is process-lifetime for now).

## Consequences

- Remote exposure hole closed: out-of-root session roots are refused with 403 even when the caller holds the API key; startup refuses non-loopback without roots.
- Local-first usage unchanged by default: empty roots = permissive locally.
- Desktop UX gains a consent gate for out-of-root project opens when roots are configured; consistent with the repo's policy-gate philosophy (everything policy-gated, gates not bypassed).
- `CONCERTO_PROJECT_ROOTS` is read in two places by design: config crate (for `AppConfig` consumers incl. the desktop) and api-server startup gate (mirroring how `CONCERTO_API_KEY`/`CONCERTO_API_HOST` are read) — documented duplication, no behavioral divergence because the server reads the same env var at startup.

## Related ADRs

- Does not supersede any ADR. Related: ADR-03 (layered config — `project_roots` follows the existing figment layering and env precedence), ADR-04 (keyring — credentials stay in the keyring, never in TOML; roots are paths, not secrets).
