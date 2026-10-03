# ADR-78: CLI settings and Studio parity

**Status:** Accepted

**Date:** 2026-10-03

## Context

The historical desktop/CLI parity plan excluded whole desktop pages because
their layout was graphical. This also excluded portable settings and actions:
agent prompts, capabilities, lifecycle tags, blueprints, provider credentials,
extension management, retry controls, and shell profiles. Six TUI settings and
read-only extension commands do not provide desktop feature parity.

## Decision

Parity applies to capabilities and persisted data. Only their graphical
presentation may be exclusive to Desktop. A graph has a textual topology;
an editor has file and command equivalents; settings remain configurable even
when their effect is visible only in Desktop. Both frontends retain the same
config, credential, agent-file, extension, and runtime service boundaries.

Add discoverable `config show/get/keys/set/unset` commands, with explicit global
or project scope and a dry-run preview. Values use TOML syntax, including arrays
and inline tables. Indexed paths address existing records. Writes preserve
unrelated comments/keys, validate the candidate through the existing load seam
before replacing the file, and reject unknown keys. Effective reads include
project/environment overrides; global reads and writes exclude those overrides.
Environment values are redacted in normal output. Secrets stay in the keychain.

Studio agent edits use the canonical per-agent file store. The CLI exposes
roster inspection/import/edit/clone/remove and advisory blueprint inspection
and selection/import. Global-only orchestration settings cannot be written as
project overrides. Agent capabilities remain requests, never policy grants.

Expose extension lifecycle, MCP connection probes, provider discovery and
credential management, and shell runtime management through the existing
services. Changes apply to the next run unless the shared runtime already
supports a live operation. A separate CLI process cannot pause or cancel a
desktop run; do not pretend that editing configuration is remote control.

Broaden the TUI's direct settings controls and link to the command surface for
structured settings. Replace the historical blanket GUI exclusions with a
capability matrix and explicit remaining work. CLI help must be available
without entering a TUI or requiring provider setup.

## Verification

Cover invalid/unknown edits without writes, comment preservation, layering,
global-only scope, authoritative agent files, structured/indexed records,
dry runs, credential-input handling, command routing, and narrow terminal
rendering. Run formatting, affected-crate tests, Clippy, and workspace checks.
