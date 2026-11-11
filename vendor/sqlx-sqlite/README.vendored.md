# Vendored SQLx SQLite driver

This is the published `sqlx-sqlite` 0.9.0 source, originating at SQLx commit
[`003b698e99e024f3621b8043a2426fde5b741171`](https://github.com/transact-rs/sqlx/tree/003b698e99e024f3621b8043a2426fde5b741171/sqlx-sqlite).
The Rust implementation is unchanged; trailing spaces were removed from three
doc-comment lines in `src/connection/deserialize.rs` for diff hygiene. The
normalized `Cargo.toml` changes only the
`libsqlite3-sys` requirement from `>=0.30.1, <0.38.0` to `0.40`. The original
upstream manifest is retained as `Cargo.toml.orig`. Full MIT/Apache license
texts come from that same upstream commit; the published driver's license
files contain only references to its parent directory.

## Why the patch exists

[RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459.html)
affects the previously linked SQLite engine: mismatched database/WAL page
sizes can disclose process memory. SQLx 0.9.0 is the latest published release
as of 2026-11-11, and its dependency cap prevents selecting a patched engine.

`libsqlite3-sys` 0.40.0 bundles patched SQLite and uses `sqlcipher-src` 420.0.0
(SQLCipher 4.20.0 / SQLite 3.54.1) for `bundled-sqlcipher`. Merely selecting
0.39.x would leave the SQLCipher path affected even though the generic
RustSec version check passes. Concerto retains SQLCipher feature unification
on non-Windows targets and its existing fail-closed encryption behavior on
Windows. This patch does not switch to an unverified system SQLite.

## Verification and retirement

The session test `sqlite_runtime_includes_wal_page_size_security_fix` queries
the engine actually linked by SQLx and requires SQLite 3.54.0 or newer. It
runs in the Linux/macOS/Windows native CI matrix. Session encryption,
migration and reopen tests cover the existing storage contracts; the full
workspace battery covers other SQLx consumers.

Remove this directory, its workspace exclusion and its `[patch.crates-io]`
entry once a published SQLx release accepts the patched SQLite/SQLCipher
dependency. Re-run the runtime version test, session tests, full CI battery
and both security scans when replacing the patch. Do not lower the runtime
version assertion to satisfy an older bundled engine.
