# ADR-37: Plugin Capability Grant Lifecycle — TTL, Hash Pinning, and Revocation

**Status:** Accepted — renumbered on 2026-08-02 because the original ADR number
was already allocated to a different ADR (durable orchestration runtime).
Content is unchanged.
**Date:** 2026-07-26
**Deciders:** Concerto architecture

## Context

WASM plugins (`concerto-plugins`) require user approval for capabilities such as
filesystem read/write, shell command execution, and network access. The current
implementation persists grants indefinitely in `plugin_cap_grants.json` with no
expiration, no link to the plugin binary version, and no revocation mechanism.

This creates a supply-chain risk: a plugin that was approved for benign use at
version 1.0 could be updated to version 1.1 (malicious) and still load with the
original grant, because the system only matches by plugin ID. The user is never
re-prompted.

The same persistence means there is no way for a user to revoke a previously
approved capability without manually editing the JSON file.

## Decision

Three changes to the grant lifecycle:

### 1. Grant TTL

- Every capability grant receives an `expires_at` UTC timestamp.
- Default TTL: **30 days** from grant creation or re-approval.
- After expiry, the grant is treated as non-existent: the plugin reverts to the
  "new capability request" prompt state and the user must re-approve.
- The TTL is checked at load time (not lazily). Expired grants are silently
  removed during `PluginManager::load_plugin` and logged at `info` level.

### 2. Manifest hash pinning

- Each grant stores `manifest_hash: Option<String>` — SHA-256 of the loaded
  WASM binary at the time of approval.
- On load: if `manifest_hash` is `Some` and the current binary hash differs,
  the grant is treated as stale. The plugin is re-prompted as if it had no
  grant, and on re-approval the new hash replaces the old one.
- `manifest_hash = None` (from migration of legacy grants) means "hash not
  established" — the plugin loads on the TTL clock alone, and on next
  re-approval the hash is pinned.

### 3. Revocation UI

- **CLI:** `concerto plugin revoke <plugin-id>` — deletes the plugin's grant
  entry from `plugin_cap_grants.json`. Next load re-prompts.
- **Desktop:** Settings → Plugins section — list of granted plugins with a
  "Revoke" button per entry. Behaves identically to the CLI command.
- Both paths log the revocation at `info` level for audit traceability.

## Schema Migration

`plugin_cap_grants.json` gains three new fields per entry:

| Field | Type | Nullable | Default |
|-------|------|----------|---------|
| `manifest_hash` | `string` | Yes | `null` |
| `created_at` | `string` (RFC 3339) | No | migration timestamp |
| `expires_at` | `string` (RFC 3339) | No | migration timestamp + 1 day |

### Migration strategy (Option A from audit):

Existing grants receive:
- `created_at` = `now() - 29 days` (so they expire ~1 day after upgrade)
- `manifest_hash` = `null` (re-prompt to establish hash)
- `expires_at` = `now() + 1 day`

This gives users a 24-hour window to re-approve plugins gracefully rather than
an immediate reset, while ensuring no grant lives indefinitely.

## Consequences

### Positive
- Supply-chain window limited to 30 days max per approval.
- Plugin updates detected via hash mismatch force re-approval.
- Users can audit and revoke grants without editing JSON manually.
- Legacy grants get a soft 24-hour migration window.

### Negative
- Users with many plugins see re-prompt cycles every 30 days. Acceptable for
  a development tool; if it becomes friction, a "trusted publisher" mechanism
  can be added later.
- CLI `plugin revoke` needs read/write access to the capability store data
  directory — consistent with existing `concerto config` subcommand patterns.
- Desktop Settings needs a new "Plugins" section with grant list/revoke.

### Risk
- System clock manipulation could extend grants. Mitigated by the 30-day cap:
  even with clock manipulation, a grant cannot outlive its original creation
  date by more than 30 days of wall time. The hash pinning catches binary
  tampering regardless of clock state.

## Alternatives Considered

- **No TTL (status quo):** simplest, but leaves the supply-chain window open
  indefinitely. Rejected.
- **Per-session grants only:** secure, but burdensome for users who use plugins
  in every session. Rejected.
- **Plugin registry signature verification:** most secure, but introduces a
  dependency on a registry and key infrastructure. Deferred to post-v1.0.

## Relationship to Other ADRs

- Supersedes the implicit grant model in ADR-21 (plugin capability system).
- Continues the wasmtime sandbox model (ADR-21) with an additional policy layer
  outside the sandbox.
- The CLI subcommand pattern follows ADR-25 (CLI subcommand structure).
