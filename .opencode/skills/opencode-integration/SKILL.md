---
name: opencode-integration
description: Integrate, build, and package the git-ignored OpenCode `opencode-local` provider patch for Concerto. Use when asked to build Windows or Linux binaries with OpenCode free-model support, apply or regenerate local-dev/patches/opencode-local.patch, run the opencode-local feature, copy artifacts to /mnt/Temp, or to understand why OpenCode's zero-cost models are unreachable from a plain HTTP client.
---

# OpenCode free-model integration

Executable sources outrank these notes. **Read this before re-diagnosing anything** —
the mechanism below was established empirically and re-deriving it is expensive.

## What this is

`opencode-local` is a Concerto provider type that talks to a running
**`opencode serve`** (OpenCode **v1** stack) over its HTTP API:

| Call | Purpose |
|---|---|
| `GET /provider` | model catalog; filtered to `cost.input == 0` |
| `POST /session` | creates a session (returns the id) |
| `POST /session/{id}/message` | sends a turn; body `model` is a **top-level object** `{"providerID":"opencode","modelID":"<model>"}` — root-level `providerID`/`modelID` are ignored |
| `DELETE /session/{id}` | teardown; leaks sessions if skipped |

HTTP Basic auth: user `opencode`, password = `OPENCODE_SERVER_PASSWORD`.

Free-ness is **cost only** (`cost.input == 0`). A model's *name* is never
consulted — `big-pickle` and `grok-code` are free with no `-free` marker.

## Why a shim is required (do not re-litigate this)

OpenCode's Zen relay gates its zero-cost models behind a **server-side client
attestation**. Measured against the live relay, all four of these are required
simultaneously:

1. `stream: true` in the body — `stream: false` → `403 FreeTierError`
2. a `tools` array containing a tool named exactly `bash` **and** one named
   exactly `read` — lower-case, exact match; count, order, descriptions and
   schemas are all irrelevant
3. an `x-opencode-session` header carrying a **server-issued** session id — a
   syntactically identical but never-observed id (e.g. `ses_` + 24 zeros)
   → `403`
4. `User-Agent: opencode/<version>` with a version the relay still accepts —
   `opencode/0.0.1` → `426 UpgradeRequired`

Omitting the fingerprint entirely (no session, no UA) drops the request to the
anonymous path → `429 FreeUsageLimitError` (per-IP cap). A populated-but-wrong
`Authorization` header does **not** help; the `oc_sk_…` API key is a different
credential universe and is refused for free models.

Condition 3 is **unforgeable**. There is no header, field, or cookie an
external client can synthesise. Therefore a direct relay client is impossible,
and a local `opencode serve` — which runs the genuine client — is the only
legitimate route. It forges nothing, so no credential is put at risk.

Two consequences worth remembering:
- Only `muse-spark-*` uses the Responses dialect; it **rejects**
  `/chat/completions`. `api_mode_for` in `crates/providers/src/opencode.rs`
  already routes it.
- `ling-3.0-flash-fin-free` is advertised by the server but 404s at the router
  (`Cannot find any route matching`). Not fixable client-side. Expect **7
  working models, not 8**.

## Layout: flag committed, implementation local

- **Committed:** the `opencode-local` feature flag (default **off**) plus every
  `#[cfg]` gate. A normal `cargo build`, CI run, or release is unaffected.
- **Git-ignored:** `local-dev/` (excluded per-clone via `.git/info/exclude`),
  holding `patches/opencode-local.patch` and `build-xwin-dev.sh`.

> The patch is **unbacked and uncommitted by design**. If it is lost it cannot be
> reconstructed from the repo — the implementation is not in git. Back it up.

## Prerequisites

```bash
OPENCODE_SERVER_PASSWORD='<password>' opencode serve --hostname 127.0.0.1 --port 4096
```

- **OpenCode v1 only.** The v2 stack reshapes all four routes (different paths,
  a `{location,data}` envelope, `cost[0].input` instead of `cost.input`, no
  `DELETE /session/{id}`) and stops reading `auth.json`, so the connector
  targets v1.
- Bind `127.0.0.1`. `opencode serve` defaults to `0.0.0.0`, which exposes it to
  the whole LAN.
- In Concerto configure provider type `opencode-local`, base URL
  `http://127.0.0.1:4096`, credential = that password. The wizard prompt labelled
  "API key" carries the server password.
- Use the **literal IPv4 address**, not `localhost`: `localhost` resolves to
  `::1` first on several systems and a `127.0.0.1`-bound server is then
  unreachable via the fallback path.

## Build Windows

```bash
./local-dev/build-xwin-dev.sh                # debug  -> target/x86_64-pc-windows-msvc/debug/concerto.exe
./local-dev/build-xwin-dev.sh -- --release   # release
./local-dev/build-xwin-dev.sh --check        # verify patch applies/builds/tests, build nothing
```

The script applies the patch, builds with the flag on across the five packages
that own or select it (`concerto`, `concerto-providers`, `concerto-config`,
`concerto-cli`, `concerto-desktop` — all are required, because `dep/feature` is
only accepted for a *selected* package), then **trap-reverts the patch** on any
exit path and asserts the tree is clean. It refuses to run on a dirty tree and
refuses if the implementation has leaked into `HEAD`.

Needs `cargo install cargo-xwin` and `rustup target add x86_64-pc-windows-msvc`.

## Build Linux

Native, no patch script — apply the patch yourself and build:

```bash
git apply local-dev/patches/opencode-local.patch
FEAT="concerto-providers/opencode-local,concerto-config/opencode-local,concerto-cli/opencode-local,concerto-desktop/opencode-local"
PKGS=(-p concerto -p concerto-providers -p concerto-config -p concerto-cli -p concerto-desktop)
cargo build --release "${PKGS[@]}" --features "$FEAT"   # -> target/release/concerto
git apply --reverse local-dev/patches/opencode-local.patch   # always revert
```

## Package artifacts to /mnt/Temp

Name each binary with the git short hash so an artifact is traceable to its
source commit. **Build from a clean tree at the commit you want to name**, and
capture the hash *before* reverting the patch.

```bash
SHORT=$(git rev-parse --short HEAD)          # e.g. 8218bf2
FEAT="concerto-providers/opencode-local,concerto-config/opencode-local,concerto-cli/opencode-local,concerto-desktop/opencode-local"
PKGS=(-p concerto -p concerto-providers -p concerto-config -p concerto-cli -p concerto-desktop)

# --- Windows ---
./local-dev/build-xwin-dev.sh -- --release
cp "target/x86_64-pc-windows-msvc/release/concerto.exe" "/mnt/Temp/concerto-${SHORT}-windows-x86_64.exe"

# --- Linux (needs the patch applied) ---
git apply local-dev/patches/opencode-local.patch
cargo build --release "${PKGS[@]}" --features "$FEAT"
cp target/release/concerto "/mnt/Temp/concerto-${SHORT}-linux-x86_64"
git apply --reverse local-dev/patches/opencode-local.patch
```

`/mnt/Temp` is a network share (`//192.168.0.18/share`), so it also serves as
off-box backup for the patch — worth doing, since the patch is otherwise the
single unbacked copy of the implementation.

Verify the Windows artifact really carries the shim:

```bash
strings -a "/mnt/Temp/concerto-${SHORT}-windows-x86_64.exe" | grep -c opencode-local
```

## Verifying a change

With the patch applied, both states must hold:

```bash
cargo check --workspace --all-targets                                   # flag OFF: clean
cargo check --workspace --all-targets --features "$FEAT" "${PKGS[@]}"   # flag ON: clean
cargo test -p concerto-providers                     # flag OFF: 542 passed
cargo test -p concerto-providers --features opencode-local   # flag ON: 575 passed (27 shim tests)
cargo clippy --workspace --all-targets --features "$FEAT" "${PKGS[@]}" -- -D warnings
```

## Regenerating the patch

Only possible while the implementation is in your working tree:

```bash
git diff -- . ':(exclude)crates/providers/Cargo.toml' \
           ':(exclude)crates/providers/src/opencode.rs' \
  > local-dev/patches/opencode-local.patch
```

`crates/providers/Cargo.toml` (the flag) and `crates/providers/src/opencode.rs`
(doc corrections) are committed and must be excluded, or the patch will not
apply to `HEAD`.

## Known rough edges

- The connector reports `supports_tool_calling: false` for every model. The
  server's HTTP API cannot carry Concerto's tool schemas (its `tools` map only
  toggles permissions over the *server's* registry), so this deliberately
  selects the ADR-66 §4 prompt-text fallback driver. Tools work, via text
  parsing. Native tool calling would need a separate design.
- `opencode_local::tests::cancellation_after_session_creation_deletes_the_session`
  is timing-sensitive: it sequences cancellation with two `tokio::task::yield_now()`
  calls and has flaked once under CPU load. Pre-existing; do not weaken it to
  make a run green.
