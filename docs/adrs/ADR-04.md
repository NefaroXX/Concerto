# ADR-04: Secure Credential Storage — `keyring`

**Status:** Accepted
**Date:** 2025-07-11
**Deciders:** Concerto architecture

## Context

API keys and other secrets must never be written to `config.toml` or any
other file on disk (Secure Credential Policy, cross-cutting requirement).
They need a storage backend that uses the OS's native credential store —
macOS Keychain, Windows Credential Manager, Linux Secret Service / kwallet —
so a stolen config file or a `cat ~/.config/concerto/config.toml` never
leaks a key. CI and unit tests must be able to exercise credential-dependent
code paths without a real keychain available (most CI runners don't have
one, or have no user session to unlock one).

## Decision

Use the `keyring` crate, service name `concerto`, account name
`<provider>/<key_name>` (e.g. `anthropic/api_key`). `config.toml` stores only
a reference shape (`{ source: "keyring", key: "concerto/openai/api_key" }`),
never the secret itself. A `CredentialStore::from_env()` test-mode
constructor reads `CONCERTO_<KEY>` env vars instead, used exclusively in
CI and `#[test]` code — production code paths always use
`CredentialStore::new()` (real keychain).

## Consequences

- No secret ever touches disk in plaintext, including in backups of the
  config directory.
- First-run setup wizard (Phase 3) writes directly to the keychain via this
  crate; nothing about the wizard needs to change if the backend changes
  later.
- `keyring`'s Linux backend depends on a running Secret Service or kwallet
  daemon; headless Linux without either will fail credential writes. Not a
  blocker for the primary desktop/dev use case; revisit if a genuinely
  headless server deployment becomes a real target.
- Test mode is read-only by design (`CredentialStore::set` in test mode
  returns an error rather than silently no-op'ing) — a test or CI job that
  tries to *write* a credential is almost certainly testing the wrong thing,
  and we want that to fail loudly, not pass by accident.

## Alternatives Considered

- **Encrypted file on disk (age, etc.):** still a file that can be copied off
  the machine; doesn't meet "never on disk" as cleanly as the OS keychain.
- **Always require env vars, no keychain:** simpler, but pushes the
  "where do I put my key" problem onto the user with no first-run wizard
  story, and env vars are visible to any process that can read `/proc` on
  Linux — weaker isolation than the OS keychain.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
