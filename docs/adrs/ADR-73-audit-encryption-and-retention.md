# ADR-73: Audit-Log Encryption at Rest and Bounded Retention

**Status:** Accepted

Answers the question [ADR-40](./ADR-40.md) §Decision item 3 deliberately left
open ("Audit retention remains a future policy question, not a session one").
Composes with ADR-40 (append-only audit log, detach-don't-delete), ADR-03
(layered config) and ADR-04 (keyring credential storage), and ADR-11 (the
data-dir lock that makes the startup prune single-writer). **Amends** ADR-40
§Decision item 3 only; ADR-40 items 1, 2 and 4 are affirmed and unchanged, and
ADR-40 itself stays in place (partial amendment, not full supersession).
Addresses security-threat-model §6 gap #5 (:339–344) and DEFERRED row 44
(which absorbed the former row 19 on 2026-09-25).

**Date:** 2026-09-26

**Deciders:** Concerto architecture + maintainer direction

**Implemented by:** commit `7351128` ("feat(sessions): encrypt the audit
database at rest and add retention"), migration
`crates/sessions/migrations/033_audit_created_at_index.sql`.

## Context

ADR-40 made `audit_log` append-only: `delete_session` stops deleting audit
rows, the session pointer is nulled instead (`ON DELETE SET NULL`), and the
decision record outlives the working data. That is the right call for
forensics and it has a direct cost — the log is grow-only by design, and
ADR-40 item 3 explicitly declined to bound it:

> **3. Audit retention remains a future policy question, not a session one.**
> No age- or size-based *audit-only* truncation is introduced; if one is ever
> wanted it belongs in its own ADR, separate from session pruning.

That is this ADR. It was also an open TODO (`docs/TODO.md:14–17`: "Define
retention/archival for `audit_log` rows").

Two gaps converge on the same store:

- **The log was plaintext at rest.** `sessions.db` held the append-only
  decision record in a plain SQLite file readable by anything with filesystem
  access to the data directory. Security-threat-model §6 gap #5 rates this
  Medium ("sensitive metadata exposed if database file accessed"). The row is
  sensitive by construction: `input_hash`, `resolved_executable`, full `argv`,
  `working_directory`, `rule_matched`, and `user_response` for every policy
  decision.
- **The log was unbounded.** Encryption alone does not fix an ever-growing
  file — it only makes an ever-growing file unreadable without the key. The
  retention bound is what makes the encrypted store a *bounded* store, which
  is why the two halves land as one decision.

ADR-40's own framing is the constraint this decision must respect: retention
is **not** a session-lifecycle concern. Session pruning still never touches
audit rows (ADR-40 item 1 is unchanged). What is being decided here is a
separate, operator-configured, **age-based** bound with an archive step — the
exact shape ADR-40 said belonged in its own ADR.

## Decision

### 1. Encryption at rest — SQLCipher, opted into, fail-closed

- **Cipher:** SQLCipher, obtained by enabling `bundled-sqlcipher` on
  `libsqlite3-sys` in the workspace `Cargo.toml` so Cargo feature unification
  gives sqlx's own `libsqlite3-sys` the SQLCipher amalgamation. The
  persistence layer stays plain sqlx — the key travels as `PRAGMA key` through
  `SqliteConnectOptions::pragma`, so sqlx applies it to every pooled
  connection. `concerto-sessions` never names a SQLCipher API; it only
  consumes the feature.
- **Fail closed, never plaintext.** `ensure_sqlcipher_build` refuses to run
  when `PRAGMA cipher_version` reports no SQLCipher: *a build that cannot
  encrypt never runs plaintext.* This includes the Windows target, where the
  `libsqlite3-sys` build script needs `OPENSSL_DIR` — the dependency is
  target-gated off Windows, and a Windows build with `encrypt_at_rest = true`
  fails with an explicit "SQLCipher unavailable" error.
- **Scope:** the whole `sessions.db` — sessions, transcripts **and** the
  append-only `audit_log`. There is no partial encryption of the log alone.
- **Key** (`AtRestKey`, `crates/sessions/src/at_rest.rs`): 32 bytes from the
  OS CSPRNG, hex-encoded. Resolution order is
  `CONCERTO_AUDIT_DB_ENCRYPTION_KEY` (env — headless provisioning and an
  explicit override) → OS keychain account `audit/db_encryption_key` →
  generate once and store. A key that can neither be read nor stored **fails
  the connect** with an actionable message; there is no plaintext fallback.
- **No key leakage into logs.** `AtRestKey`'s `Debug` is redacted, and
  statement logging is disabled on keyed connections so `PRAGMA key` can never
  reach a log sink.
- **Marker as offline proof:** a sidecar `<db>.sqlcipher` states "this
  database is meant to be encrypted". It (a) keeps the ADR-54 corruption
  heuristic — which only understands plaintext SQLite headers — from
  quarantining a healthy encrypted file, (b) makes an interrupted
  plaintext→encrypted swap recoverable, and (c) is what `connect_path` checks
  before opening plaintext. A wrong key on a marked file errors with the file
  untouched: ADR-54 guards corruption, not encryption, and a failed key must
  not trigger a quarantine-and-rebuild.
- **Health:** `concerto health` reports `ok (encrypted at rest)` for a marked
  database instead of `corrupt (rebuilt on next open)`.
- **Migration is crash-safe:** an existing plaintext database is exported with
  `ATTACH … KEY` + `sqlcipher_export` into `<db>.enc-tmp`, verified under the
  key, refused-for-swap while `-wal`/`-shm`/`-journal` show another live
  connection, marked, then renamed. `recover_interrupted_migration` finishes or
  rolls back each of the four interrupted states, and leftovers are swept.

### 2. Retention — archive first, then delete, verified

`SqliteSessionStore::prune_audit(retention_days, archive_dir, cancel)`
(`crates/sessions/src/audit_retention.rs`):

- **Archive, then delete, one transaction.** Rows strictly older than the
  cutoff are copied into `<archive_dir>/audit-archive.db` (ATTACHed, schema
  `audit_log`'s columns plus `archived_at`, no foreign keys — archived rows
  deliberately outlive sessions per ADR-40) and only then deleted from the hot
  `audit_log`. SQLite's transaction spans both databases, so a crash rolls
  both back; a crash that somehow landed between the two converges on the
  next prune (`INSERT OR IGNORE` on the primary key plus a
  "nothing about to be deleted is missing from the archive" check).
- **Verify before delete.** A failed archive write — missing directory, full
  disk, wrong key — aborts **before** the delete. Audit rows are never
  removed without a copy.
- **The archive gets the same key.** It is attached with the store's at-rest
  key, so enabling encryption never leaves old audit rows in a plaintext file.
- **`retention_days = 0` is disabled, never "delete everything".** A `0`
  config value is folded away at the config seam and `prune_audit` rejects
  `0` outright, so a direct caller cannot wipe the log by accident.
- **Cancellable.** `cancel` is checked before every statement.
- **Trigger:** `connect()` runs a **best-effort** prune while it holds the
  data-dir lock (ADR-11), so two instances cannot prune concurrently. A
  failing prune only warns and never blocks startup — the worst case is an
  unpruned log, since rows are removed only after archiving.
- **Predicate support:** migration `033_audit_created_at_index.sql` indexes
  `audit_log(created_at)` for the time predicate.

### 3. Policy surface — `[audit]`, config-driven v1

A new **additive** `[audit]` config section (`crates/config/src/schema.rs`,
`AuditConfig`): `Option`-wrapped with `#[serde(default)]`, following the
ADR-70 precedent, so **no `SCHEMA_VERSION` bump** (it stays `8`) and no
existing config changes meaning.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `encrypt_at_rest` | `bool` | `false` | Encrypt `sessions.db` with SQLCipher. |
| `retention_days` | `Option<u64>` | `None` (keep forever) | Days a row is retained in the hot log before it is archived out and deleted. `None` or `0` keeps every row. |
| `archive_dir` | `Option<Utf8PathBuf>` | `None` → the data directory | Where `audit-archive.db` is written. |

- **Global config file only** — no env layer, no project file. At-rest
  encryption is machine policy; it must not be flip-flopped by a per-project
  override or an ambient `CONCERTO_*` variable. A config that fails to load
  **fails the connect**: silently running plaintext because settings could not
  be read would defeat the feature.
- **Config-driven v1:** no Settings UI surface, no CLI command. Same shape as
  the ADR-43 `[skills]` / `[mcp]` sections (next-run semantics).
- **Defaults are conservative on both axes, in the ADR-40-preserving
  direction:** encryption is opt-in, and retention is **off** — so a
  default install keeps ADR-40's exact "no age-based truncation" behavior and
  a plaintext database, while an operator who sets `encrypt_at_rest` gets
  fail-closed encryption and an operator who sets `retention_days` gets
  archive-then-delete. Neither feature is silently imposed.

### 4. Scope of the amendment to ADR-40

| ADR-40 clause | Disposition here |
|---|---|
| Item 1 — `audit_log` is append-only, never trimmed by session lifecycle | **Unchanged.** Session pruning still never deletes audit rows. |
| Item 2 — detach, don't delete (`ON DELETE SET NULL`) | **Unchanged.** |
| Item 3 — "audit retention remains a future policy question" | **Superseded by this ADR.** The question is now answered: age-based, archive-then-delete, operator-configured, and — as ADR-40 required — still *not* a session concern. |
| Item 4 — migration `021` table rebuild mechanics | **Unchanged.** |

The distinction ADR-40 insisted on is preserved: retention is a **time-based
policy**, not a **session-lifecycle** consequence. Pruning a session still
does not touch the log; aging out a row still does not delete the session.

### 5. Integration consequence (recorded because it is load-bearing)

The orchestrator's audit sink keeps its own connection pool. It now opens
through `SqliteSessionStore::open_pool` (`crates/orchestrator/src/runtime_runner.rs`),
which applies the same at-rest policy (key resolution and plaintext→encrypted
migration included). Without that, an encrypted database would have failed that
open and **silently disabled audit recording and the write gate** — the exact
security regression this ADR exists to close.

## Consequences

- **The decision record is protected at rest, on operator demand, and never
  degrades silently.** Every failure mode on the enabled path is an error, not
  a fallback: unavailable SQLCipher, unreadable/unstorable key, wrong key on a
  marked file, another live connection during the swap.
- **The hot log is bounded by policy.** Retention moves aged rows into an
  encrypted archive instead of leaving them to accumulate in `sessions.db`
  forever, and nothing is ever deleted before it is archived. The *archive*
  itself is not bounded by this ADR (see Residuals).
- **ADR-40's forensic promise is intact** for the archived set: archived rows
  keep the full decision facts, and `session_id` stays nullable in the archive
  (no foreign keys).
- **New operational surface:** an `[audit]` config section, an
  `audit-archive.db` file to back up/protect alongside `sessions.db`, and a
  `sessions.db.sqlcipher` marker that must travel with the database.
- **Costs / risks:** a lost key is unrecoverable data (no escrow or rotation
  story yet); the archive is a single growing file this ADR does not bound;
  retention only runs on `connect()`, so a long-lived process prunes at its
  next connect; and encryption being opt-in means a default install is still
  plaintext until an operator enables it.

## Verification notes (in tree, 2026-09-26)

- Retention unit tests in `crates/sessions/src/lib.rs`:
  `prune_audit_archives_and_deletes_only_old_rows`,
  `prune_audit_cutoff_is_strictly_older`,
  `prune_audit_rejects_zero_days_and_honours_cancellation`,
  `prune_audit_never_deletes_when_the_archive_is_unavailable`,
  `prune_audit_writes_an_encrypted_archive_when_the_store_is_encrypted`,
  `prune_audit_keeps_everything_on_an_absurd_window`.
- `crates/sessions/src/at_rest.rs` covers key hex round-trip, bad-key
  rejection, `Debug` redaction, key uniqueness, marker detection, and the
  interrupted-migration recovery states (`recover_finishes_swap_when_main_file_missing`,
  `recover_restores_old_when_export_is_gone`, `recover_drops_orphan_marker`, …).
- End-to-end in `crates/sessions/src/lib.rs`:
  `at_rest_encrypts_a_fresh_database_and_fails_closed_on_wrong_key` (plaintext
  path refuses a marked file; a wrong key errors with the file and marker
  untouched and never quarantines),
  `at_rest_migrates_an_existing_plaintext_database`,
  `at_rest_restores_a_lost_marker`, `at_rest_recovers_a_stale_marker_and_converts`.
- `crates/cli/src/health.rs` asserts the `ok (encrypted at rest)` state for a
  marked database.
- `AuditSettings::from_config` is a pure projection and is unit tested
  (`audit_settings_defaults_keep_status_quo`,
  `audit_settings_fold_zero_retention_and_paths`).
- Commit `7351128` reported: `cargo fmt --all -- --check`, clippy
  `-D warnings` (workspace, all targets), full workspace tests green
  (sessions 144, config 255, cli 194, orchestrator 1340+, memory 277 with
  FTS5 under SQLCipher), and `cargo deny check`.
- CLI prune tests isolate `XDG_CONFIG_HOME` so `connect()` sees default
  config rather than a developer's real `[audit]` section.

## Residuals

- **Rotation and escrow are not covered.** There is no key-rotation, key
  escrow, or re-key procedure; a lost key means an unreadable database. A
  future ADR should own this, and it is the strongest reason to delay making
  `encrypt_at_rest` the default.
- **The archive is unbounded.** `audit-archive.db` grows forever and this ADR
  gives it no lifecycle of its own (a nested retention tier, an operator
  "prune the archive" command, or a documented external rotation job).
- **No read/query path for the archive.** The audit log is still write-only;
  this ADR does not add an archive reader.
- **No size-based bound** — retention is age-based only, as ADR-40 framed the
  question.
- **Policy surface is config-only**: no Settings UI, no CLI command, and no
  per-session override (deliberately — see §3).
- **Windows cannot enable `encrypt_at_rest`** until the SQLCipher/OpenSSL
  build story is solved; the failure is explicit, not silent.

---

*Last updated: 2026-09-26*
