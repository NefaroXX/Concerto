//! At-rest encryption for the sessions/audit database (DEFERRED row 44;
//! security threat model gap #5, `docs/security-threat-model.md` §6).
//!
//! Design, in one place:
//!
//! * **Cipher.** SQLCipher, obtained by enabling `bundled-sqlcipher` on
//!   `libsqlite3-sys` (workspace `Cargo.toml`) so Cargo feature unification
//!   gives sqlx's own `libsqlite3-sys` the SQLCipher amalgamation. The
//!   persistence layer stays plain sqlx: the key travels as `PRAGMA key`
//!   through [`SqliteConnectOptions::pragma`], and sqlx applies it to every
//!   pooled connection.
//! * **Key.** 32 random bytes, hex-encoded for the pragma. Resolution order:
//!   `CONCERTO_AUDIT_DB_ENCRYPTION_KEY` (env — headless provisioning and
//!   overrides) → OS keychain account [`AT_REST_KEY_ACCOUNT`] → generate once
//!   and store. A key that cannot be read *or* stored fails closed with an
//!   actionable message; there is no plaintext fallback.
//! * **Marker.** A sidecar file `<db>.sqlcipher` states "this database is
//!   meant to be encrypted". It exists so that (a) the ADR-54 quarantine
//!   heuristic (which only understands plaintext SQLite headers) never moves a
//!   healthy encrypted file, and (b) a plaintext → encrypted swap interrupted
//!   by a crash can be finished or rolled back on the next connect.
//! * **Migration.** An existing plaintext database is converted with
//!   `sqlcipher_export` into `<db>.enc-tmp`, verified with the key, marked,
//!   and swapped in with two renames; the previous file survives briefly as
//!   `<db>.old` and is swept after the first successful keyed open.
//! * **Logging.** Statement logging is disabled for keyed connections: sqlx
//!   logs statement text at `Debug`, and the key pragma must never reach a
//!   log sink. [`AtRestKey`]'s `Debug` output is redacted as well.
//!
//! Encryption is opt-in (`[audit] encrypt_at_rest`, default off) and covers
//! the whole `sessions.db`: sessions, transcripts *and* the append-only
//! `audit_log`.

use std::fmt;
use std::path::{Path, PathBuf};

use concerto_config::credentials::CredentialStore;
use concerto_config::AppConfig;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{AssertSqlSafe, ConnectOptions as _, Connection, SqliteConnection, SqlitePool};

use crate::{run_migrations, SessionError, SqliteSessionStore};

/// Keychain account holding the at-rest key. The env override derived from it
/// is `CONCERTO_AUDIT_DB_ENCRYPTION_KEY` (`CredentialStore` uppercases and
/// replaces `/` with `_`).
pub const AT_REST_KEY_ACCOUNT: &str = "audit/db_encryption_key";

/// Contents of the sidecar marker written next to an encrypted database.
const MARKER_CONTENT: &str = "concerto-sqlcipher-v1\n";

// ---------------------------------------------------------------------------
// Key
// ---------------------------------------------------------------------------

/// The 32-byte at-rest encryption key (`PRAGMA key`), hex-encoded.
///
/// `Debug` is redacted so a stray `{:?}` in a log line can never leak the key.
#[derive(Clone, PartialEq, Eq)]
pub struct AtRestKey {
    raw: [u8; 32],
}

impl AtRestKey {
    /// Generate a fresh key from the OS CSPRNG.
    pub fn generate() -> Result<Self, SessionError> {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(|e| {
            SessionError::Storage(format!("failed to generate the at-rest key: {e}"))
        })?;
        Ok(Self { raw })
    }

    /// Parse the 64-hex-character form used by the keychain, the env var and
    /// `PRAGMA key`.
    pub fn parse_hex(input: &str) -> Result<Self, SessionError> {
        let trimmed = input.trim();
        if trimmed.len() != 64 {
            return Err(SessionError::Storage(format!(
                "invalid at-rest key for account '{AT_REST_KEY_ACCOUNT}': \
                 expected 64 hex characters, got {}",
                trimmed.len()
            )));
        }
        let mut raw = [0u8; 32];
        for (byte, pair) in raw.iter_mut().zip(trimmed.as_bytes().chunks(2)) {
            let hi = hex_nibble(pair[0]).ok_or_else(invalid_hex)?;
            let lo = hex_nibble(pair[1]).ok_or_else(invalid_hex)?;
            *byte = (hi << 4) | lo;
        }
        Ok(Self { raw })
    }

    /// Hex encoding of the raw key (lowercase, 64 characters).
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.raw {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Value for `.pragma("key", …)`: a single-quoted hex literal. Quoting is
    /// what SQLCipher expects; the value is hex-only, so no escaping issue.
    pub(crate) fn pragma_value(&self) -> String {
        format!("'{}'", self.to_hex())
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid_hex() -> SessionError {
    SessionError::Storage(format!(
        "invalid at-rest key for account '{AT_REST_KEY_ACCOUNT}': \
         expected 64 hex characters"
    ))
}

impl fmt::Debug for AtRestKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AtRestKey(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// `[audit]` settings
// ---------------------------------------------------------------------------

/// The `[audit]` section as `connect()` consumes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AuditSettings {
    /// `[audit] encrypt_at_rest` (default `false`).
    pub encrypt_at_rest: bool,
    /// `[audit] retention_days`, with `0` folded away (config-level
    /// "disabled" value; [`SqliteSessionStore::prune_audit`] rejects `0` so
    /// direct callers fail fast instead of deleting everything).
    pub retention_days: Option<u64>,
    /// `[audit] archive_dir`; `None` means the app data directory.
    pub archive_dir: Option<PathBuf>,
}

impl AuditSettings {
    /// Project an [`AppConfig`] onto the settings (pure — unit tested).
    pub(crate) fn from_config(config: AppConfig) -> Self {
        let Some(audit) = config.audit else {
            return Self::default();
        };
        Self {
            encrypt_at_rest: audit.encrypt_at_rest,
            retention_days: audit.retention_days.filter(|days| *days > 0),
            archive_dir: audit.archive_dir.map(|path| path.into_std_path_buf()),
        }
    }

    /// Load the *global* config (file + defaults only — no env layer, no
    /// project file): at-rest encryption is machine policy that must not be
    /// flip-flopped by per-project overrides or ambient `CONCERTO_*` vars.
    ///
    /// A config that fails to load fails the connect: silently running
    /// plaintext because the settings could not be read would defeat the
    /// feature.
    pub(crate) fn from_global_config() -> Result<Self, SessionError> {
        let config = concerto_config::load_global_config(None).map_err(|e| {
            SessionError::Storage(format!("failed to load config for [audit] settings: {e}"))
        })?;
        Ok(Self::from_config(config))
    }
}

// ---------------------------------------------------------------------------
// Key resolution
// ---------------------------------------------------------------------------

/// Resolve the at-rest key: env override → keychain → generate and store.
pub(crate) fn resolve_at_rest_key() -> Result<AtRestKey, SessionError> {
    // Environment first: explicit provisioning (headless hosts without a
    // Secret Service) and one-off overrides must win over the keychain.
    let env = CredentialStore::from_env();
    if let Ok(raw) = env.get(AT_REST_KEY_ACCOUNT) {
        if !raw.trim().is_empty() {
            return AtRestKey::parse_hex(&raw);
        }
    }

    let keychain = CredentialStore::new();
    if let Ok(raw) = keychain.get(AT_REST_KEY_ACCOUNT) {
        return AtRestKey::parse_hex(&raw);
    }

    // First run: mint a key and persist it. A keychain that cannot store the
    // key must fail the connect — otherwise the database would be written
    // with a key nobody can recover next run.
    let key = AtRestKey::generate()?;
    keychain.set(AT_REST_KEY_ACCOUNT, &key.to_hex()).map_err(|e| {
        SessionError::Storage(format!(
            "could not store the at-rest encryption key ({e}); set \
             CONCERTO_AUDIT_DB_ENCRYPTION_KEY in the environment, fix the OS \
             keychain, or disable [audit] encrypt_at_rest"
        ))
    })?;
    tracing::info!("generated a new at-rest key for the sessions database");
    Ok(key)
}

// ---------------------------------------------------------------------------
// Marker + sibling paths
// ---------------------------------------------------------------------------

/// Path of the sidecar marker declaring `db_path` at-rest encrypted.
pub fn marker_path(db_path: &Path) -> PathBuf {
    with_suffix(db_path, ".sqlcipher")
}

/// Whether `db_path` carries the at-rest marker (i.e. it is a database that
/// must be opened keyed — the ADR-54 header heuristic does not apply).
pub fn is_at_rest_encrypted(db_path: &Path) -> bool {
    marker_path(db_path).exists()
}

/// Refuse a plaintext handle to a marker-carrying database. Used by every
/// plaintext open path so a healthy encrypted file is never opened (or, via
/// ADR-54, quarantined) with the wrong policy.
pub(crate) fn ensure_not_at_rest_encrypted(db_path: &Path) -> Result<(), SessionError> {
    if !is_at_rest_encrypted(db_path) {
        return Ok(());
    }
    Err(SessionError::Storage(format!(
        "{} is at-rest encrypted (marker {} present); enable [audit] \
         encrypt_at_rest — or set CONCERTO_AUDIT_DB_ENCRYPTION_KEY — to open it",
        db_path.display(),
        marker_path(db_path).display()
    )))
}

fn with_suffix(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn tmp_path(db_path: &Path) -> PathBuf {
    with_suffix(db_path, ".enc-tmp")
}

fn old_path(db_path: &Path) -> PathBuf {
    with_suffix(db_path, ".old")
}

fn write_marker(db_path: &Path) -> Result<(), SessionError> {
    let marker = marker_path(db_path);
    std::fs::write(&marker, MARKER_CONTENT).map_err(|e| {
        SessionError::Storage(format!("failed to write at-rest marker {}: {e}", marker.display()))
    })
}

/// SQL string literal: single quotes doubled.
pub(crate) fn sql_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Connect options
// ---------------------------------------------------------------------------

/// Options for opening a plaintext database (also the *source* side of the
/// plaintext → encrypted migration, which must not create anything).
pub(crate) fn plain_options(db_path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(db_path)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal)
}

/// Options for opening an encrypted database: the key pragma plus statement
/// logging off, so `PRAGMA key = '…'` can never appear in a log line (sqlx
/// logs statement text at `Debug` by default).
fn keyed_options(db_path: &Path, key: &AtRestKey) -> SqliteConnectOptions {
    plain_options(db_path).pragma("key", key.pragma_value()).disable_statement_logging()
}

// ---------------------------------------------------------------------------
// SQLCipher availability
// ---------------------------------------------------------------------------

/// Fail closed when this build links a plain SQLite without SQLCipher.
///
/// Without this check an encrypted file surfaces as an opaque "file is not a
/// database" *after* classification — and, worse, a build that cannot encrypt
/// could otherwise appear to work by silently writing plaintext.
async fn ensure_sqlcipher_build() -> Result<(), SessionError> {
    let options = SqliteConnectOptions::new().in_memory(true).disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.map_err(|e| {
        SessionError::Database(format!("failed to probe SQLCipher availability: {e}"))
    })?;
    let outcome = fetch_cipher_version(&mut conn).await;
    let closed = conn.close().await.map_err(|e| SessionError::Database(e.to_string()));
    let version = outcome?;
    closed?;
    match version {
        Some(version) => {
            tracing::debug!(sqlcipher = %version, "SQLCipher is available");
            Ok(())
        }
        None => Err(SessionError::Storage(
            "[audit] encrypt_at_rest is enabled, but this build has no SQLCipher \
             support (PRAGMA cipher_version came back empty). Rebuild with the \
             workspace's bundled-sqlcipher feature (non-Windows targets) or \
             disable [audit] encrypt_at_rest — running plaintext instead is not \
             an option."
                .to_string(),
        )),
    }
}

/// `Some(version)` when SQLCipher is compiled in, `None` otherwise.
async fn fetch_cipher_version(conn: &mut SqliteConnection) -> Result<Option<String>, SessionError> {
    let row: Option<Option<String>> = sqlx::query_scalar("PRAGMA cipher_version;")
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| SessionError::Database(e.to_string()))?;
    Ok(row.flatten().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
}

// ---------------------------------------------------------------------------
// Migration: plaintext → encrypted
// ---------------------------------------------------------------------------

/// Convert a plaintext `db_path` to SQLCipher encryption in place.
///
/// Order of operations is crash-safe: the export lands in `.enc-tmp` and is
/// verified with the key *before* the marker is written; only then are the
/// two renames performed. `recover_interrupted_migration` finishes or rolls
/// back any of the three crash windows on the next connect.
async fn migrate_plaintext_to_encrypted(
    db_path: &Path,
    key: &AtRestKey,
) -> Result<(), SessionError> {
    let tmp = tmp_path(db_path);
    remove_file_if_exists(&tmp)?;
    remove_file_if_exists(&with_suffix(&tmp, "-wal"))?;
    remove_file_if_exists(&with_suffix(&tmp, "-shm"))?;

    export_to_tmp(db_path, &tmp, key).await?;
    verify_keyed(&tmp, key).await.map_err(|e| {
        // The original file is untouched at this point; drop only the copy.
        let _ = std::fs::remove_file(&tmp);
        SessionError::Storage(format!("at-rest encryption verification failed: {e}"))
    })?;

    // `export_to_tmp` closed its connection cleanly, which folds the WAL into
    // the main file and removes the sidecars — unless another connection still
    // holds the database. Abort *before* the marker/rename swap in that case:
    // nothing has been touched yet, and the next connect retries.
    for sidecar in ["-wal", "-shm", "-journal"] {
        let sidecar = with_suffix(db_path, sidecar);
        if sidecar.exists() {
            return Err(SessionError::Storage(format!(
                "refusing to encrypt {}: {} still exists after a clean close \
                 (another process is using the database?)",
                db_path.display(),
                sidecar.display()
            )));
        }
    }

    // Commit the swap. The marker goes first: it is what tells a later
    // `connect_path` (or a crash recovery) that the plaintext-looking file is
    // mid-conversion instead of healthy plaintext.
    write_marker(db_path)?;
    std::fs::rename(db_path, old_path(db_path)).map_err(|e| {
        SessionError::Storage(format!("failed to move plaintext database aside: {e}"))
    })?;
    std::fs::rename(&tmp, db_path)
        .map_err(|e| SessionError::Storage(format!("failed to install encrypted database: {e}")))?;
    Ok(())
}

/// `ATTACH` a keyed temporary copy and run `sqlcipher_export` into it.
async fn export_to_tmp(db_path: &Path, tmp: &Path, key: &AtRestKey) -> Result<(), SessionError> {
    let mut source =
        SqliteConnection::connect_with(&plain_options(db_path).create_if_missing(true))
            .await
            .map_err(|e| {
                SessionError::Database(format!(
                    "failed to open plaintext database for encryption: {e}"
                ))
            })?;

    let outcome = async {
        // Fold every committed WAL page into the main file first: the export
        // reads through this connection either way, but a leftover `-wal`
        // after the swap would be read as the WAL of the *encrypted* file.
        exec_sql(&mut source, "PRAGMA wal_checkpoint(TRUNCATE);").await?;
        let attach = format!(
            "ATTACH DATABASE {} AS encrypted KEY {}",
            sql_quote(&tmp.display().to_string()),
            sql_quote(&key.to_hex())
        );
        sqlx::query(AssertSqlSafe(attach.as_str()))
            .execute(&mut source)
            .await
            .map_err(|e| SessionError::Database(format!("failed to attach export target: {e}")))?;
        exec_sql(&mut source, "SELECT sqlcipher_export('encrypted');").await?;
        exec_sql(&mut source, "DETACH DATABASE encrypted;").await
    }
    .await;

    let closed = source.close().await.map_err(|e| SessionError::Database(e.to_string()));
    outcome.and(closed.map(|_| ()))
}

/// Open `path` with `key` and read the header, proving the file really is
/// the encrypted database we just produced (or found).
async fn verify_keyed(path: &Path, key: &AtRestKey) -> Result<(), SessionError> {
    let mut conn =
        SqliteConnection::connect_with(&keyed_options(path, key)).await.map_err(|e| {
            SessionError::Database(format!("keyed open of {} failed: {e}", path.display()))
        })?;
    let outcome: Result<(), SessionError> = async {
        let _: i64 = sqlx::query_scalar("PRAGMA schema_version;")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        Ok(())
    }
    .await;
    let closed = conn.close().await.map_err(|e| SessionError::Database(e.to_string()));
    outcome.and(closed.map(|_| ()))
}

pub(crate) async fn exec_sql(conn: &mut SqliteConnection, sql: &str) -> Result<(), SessionError> {
    // `AssertSqlSafe`: every caller passes either a `&'static str` literal or
    // a statement built from quoted/bound-safe fragments inside this module.
    sqlx::query(AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|e| SessionError::Database(format!("`{}` failed: {e}", sql.trim_end_matches(';'))))
}

fn remove_file_if_exists(path: &Path) -> Result<(), SessionError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SessionError::Storage(format!("failed to remove {}: {e}", path.display()))),
    }
}

// ---------------------------------------------------------------------------
// Interrupted-migration recovery
// ---------------------------------------------------------------------------

/// Finish or roll back a plaintext → encrypted swap interrupted by a crash.
///
/// Crash windows (marker is written before the renames, so its presence
/// brackets the swap):
///
/// 1. Marker + plaintext main file — crashed before the renames: drop the
///    stale marker and let the next connect re-run the migration.
/// 2. Marker + no main file + `.enc-tmp` — crashed between the renames with
///    the export already verified: install the encrypted copy.
/// 3. Marker + no main file + only `.old` — the encrypted copy vanished
///    before it was installed: restore the plaintext original and drop the
///    marker so it is treated as plaintext again.
/// 4. Marker with nothing else — nothing recoverable: drop the marker.
pub(crate) fn recover_interrupted_migration(db_path: &Path) -> Result<(), SessionError> {
    let marker = marker_path(db_path);
    if !marker.exists() {
        return Ok(());
    }

    if db_path.is_file() {
        if concerto_core::helpers::is_sqlite_file(db_path) {
            // Case 1: the marker promises encryption but the file is healthy
            // plaintext — the swap never happened.
            remove_file_if_exists(&marker)?;
            tracing::info!(
                path = %db_path.display(),
                "cleared a stale at-rest marker left by an interrupted encryption migration"
            );
        }
        return Ok(());
    }

    let tmp = tmp_path(db_path);
    if tmp.is_file() {
        std::fs::rename(&tmp, db_path).map_err(|e| {
            SessionError::Storage(format!("failed to complete interrupted at-rest swap: {e}"))
        })?;
        tracing::warn!(path = %db_path.display(), "completed an interrupted at-rest encryption swap");
        return Ok(());
    }

    let old = old_path(db_path);
    if old.is_file() {
        std::fs::rename(&old, db_path).map_err(|e| {
            SessionError::Storage(format!("failed to restore database from interrupted swap: {e}"))
        })?;
        remove_file_if_exists(&marker)?;
        tracing::warn!(path = %db_path.display(), "rolled back an interrupted at-rest encryption swap");
        return Ok(());
    }

    // Case 4.
    remove_file_if_exists(&marker)?;
    tracing::warn!(
        path = %db_path.display(),
        "at-rest marker present without any database file; cleared the marker"
    );
    Ok(())
}

/// Best-effort sweep of swap leftovers after a successful keyed open.
fn sweep_migration_leftovers(db_path: &Path) {
    for suffix in [
        ".old",
        ".old-wal",
        ".old-shm",
        ".old-journal",
        ".enc-tmp",
        ".enc-tmp-wal",
        ".enc-tmp-shm",
        ".enc-tmp-journal",
    ] {
        let path = with_suffix(db_path, suffix);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::debug!(path = %path.display(), "swept at-rest migration leftover"),
            Err(e) => {
                tracing::warn!(path = %path.display(), %e, "failed to sweep at-rest leftover")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pool construction
// ---------------------------------------------------------------------------

/// Build a pool against an already-classified database: connect, read the
/// header so a bad file fails here (not on the first query), apply the same
/// PRAGMAs the plaintext path uses, then run the schema migrations.
pub(crate) async fn build_pool(options: SqliteConnectOptions) -> Result<SqlitePool, SessionError> {
    let pool = PoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .map_err(|e| SessionError::Database(e.to_string()))?;

    let _: i64 = sqlx::query_scalar("PRAGMA schema_version;")
        .fetch_one(&pool)
        .await
        .map_err(|e| SessionError::Database(e.to_string()))?;

    for pragma in [
        "PRAGMA journal_mode=WAL;",
        "PRAGMA busy_timeout = 5000;",
        "PRAGMA foreign_keys = ON;",
        "PRAGMA synchronous = NORMAL;",
    ] {
        sqlx::query(pragma)
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(format!("`{pragma}` failed: {e}")))?;
    }

    run_migrations(&pool).await?;
    Ok(pool)
}

/// Classify `db_path` against `key`, converting an existing plaintext
/// database on the fly, and return a healthy keyed pool. On success the
/// marker exists and migration leftovers are swept.
async fn open_keyed_pool(db_path: &Path, key: &AtRestKey) -> Result<SqlitePool, SessionError> {
    ensure_sqlcipher_build().await?;
    recover_interrupted_migration(db_path)?;
    let marked = is_at_rest_encrypted(db_path);

    let pool = if !db_path.is_file() {
        // Fresh database: the first keyed connection creates it encrypted.
        build_pool(keyed_options(db_path, key).create_if_missing(true)).await?
    } else if !marked && concerto_core::helpers::is_sqlite_file(db_path) {
        // Existing plaintext database (also the post-recovery case-1 state).
        migrate_plaintext_to_encrypted(db_path, key).await?;
        build_pool(keyed_options(db_path, key).create_if_missing(true)).await?
    } else {
        // Marker says encrypted, or the file is not a SQLite file at all
        // (possibly an encrypted database whose marker was lost). The keyed
        // open is the only honest classifier.
        match build_pool(keyed_options(db_path, key).create_if_missing(true)).await {
            Ok(pool) => pool,
            Err(original) => {
                if marked {
                    // Deliberately encrypted: wrong or lost key. Quarantine
                    // must never run here (ADR-54 guards corruption, not
                    // encryption) — fail with the real cause instead.
                    return Err(SessionError::Storage(format!(
                        "failed to open {} with the at-rest key: {original}. Check \
                         CONCERTO_AUDIT_DB_ENCRYPTION_KEY / the keychain entry \
                         '{AT_REST_KEY_ACCOUNT}'; the file was left untouched.",
                        db_path.display()
                    )));
                }
                // Unmarked and unopenable: ADR-54 quarantine (bytes are
                // preserved in `.corrupt-<ts>.bak`, never deleted) then a
                // fresh encrypted database.
                match concerto_core::helpers::quarantine_corrupt_db_file(db_path) {
                    Some(quarantine) => {
                        tracing::warn!(
                            path = %db_path.display(),
                            quarantine = %quarantine.display(),
                            "unmarked at-rest database was neither valid SQLite nor \
                             openable with the key; quarantined and starting fresh \
                             (if it was an encrypted database, check the key and the \
                             .bak file)"
                        );
                        build_pool(keyed_options(db_path, key).create_if_missing(true)).await?
                    }
                    None => return Err(original),
                }
            }
        }
    };

    // The file is encrypted and healthy from here on.
    if !marked {
        write_marker(db_path)?;
    }
    sweep_migration_leftovers(db_path);
    Ok(pool)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

impl SqliteSessionStore {
    /// Connect to an explicit database path with at-rest encryption: the file
    /// at `db_path` is opened (or created, or migrated from plaintext) with
    /// SQLCipher and `key`.
    ///
    /// Unlike [`Self::connect_path`] this path never returns a plaintext
    /// handle: a wrong `key` fails closed and the database is left untouched.
    pub async fn connect_with_at_rest(
        db_path: &std::path::Path,
        key: &AtRestKey,
    ) -> Result<Self, SessionError> {
        let pool = open_keyed_pool(db_path, key).await?;
        Ok(Self { pool, _data_dir_lock: None, at_rest_key: Some(key.clone()) })
    }

    /// Open an additional pool to `db_path` honoring the same `[audit]`
    /// at-rest policy as [`Self::connect`] (key resolution and
    /// plaintext → encrypted migration included, schema migrations included).
    ///
    /// For writers that deliberately keep their own connection pool — the
    /// orchestrator's audit sink, for example — so an encrypted sessions
    /// database does not silently disable them. The returned pool carries no
    /// process-lifetime lock, exactly like `connect_path`.
    pub async fn open_pool(db_path: &std::path::Path) -> Result<SqlitePool, SessionError> {
        let settings = AuditSettings::from_global_config()?;
        if settings.encrypt_at_rest {
            let key = resolve_at_rest_key()?;
            return open_keyed_pool(db_path, &key).await;
        }
        ensure_not_at_rest_encrypted(db_path)?;
        build_pool(plain_options(db_path).create_if_missing(true)).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fake_sqlite(path: &Path) {
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend_from_slice(&[0u8; 400]);
        std::fs::write(path, bytes).expect("write fake sqlite file");
    }

    fn write_fake_encrypted(path: &Path) {
        let mut bytes = vec![0xA5u8; 512];
        bytes[0] = 0x53; // arbitrary non-SQLite header
        std::fs::write(path, bytes).expect("write fake encrypted file");
    }

    #[test]
    fn at_rest_key_roundtrips_hex() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let key = AtRestKey::parse_hex(hex).expect("parse");
        assert_eq!(key.to_hex(), hex);
        // Uppercase input normalises to lowercase.
        let upper = hex.to_uppercase();
        let key = AtRestKey::parse_hex(&upper).expect("parse upper");
        assert_eq!(key.to_hex(), hex);
        // Surrounding whitespace is tolerated (keychain/env values).
        assert_eq!(AtRestKey::parse_hex(&format!("  {hex}\n")).expect("trim").to_hex(), hex);
    }

    #[test]
    fn at_rest_key_rejects_bad_input() {
        for bad in ["", "abc", &"0".repeat(63), &"0".repeat(65), &"z".repeat(64)] {
            assert!(AtRestKey::parse_hex(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn at_rest_key_debug_is_redacted() {
        let key = AtRestKey::parse_hex(&"ab".repeat(32)).expect("parse");
        let debug = format!("{key:?}");
        assert!(debug.contains("redacted"), "Debug must be redacted, got {debug}");
        assert!(!debug.contains("abab"), "Debug must not contain key bytes: {debug}");
    }

    #[test]
    fn at_rest_key_generate_is_unique() {
        let a = AtRestKey::generate().expect("generate");
        let b = AtRestKey::generate().expect("generate");
        assert_ne!(a.to_hex(), b.to_hex(), "two generated keys must differ");
    }

    #[test]
    fn marker_detection_tracks_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        assert!(!is_at_rest_encrypted(&db));
        write_fake_encrypted(&db);
        assert!(!is_at_rest_encrypted(&db), "file alone must not imply encryption");
        write_marker(&db).expect("marker");
        assert!(is_at_rest_encrypted(&db));
        assert!(ensure_not_at_rest_encrypted(&db).is_err());
        std::fs::remove_file(marker_path(&db)).expect("remove marker");
        assert!(ensure_not_at_rest_encrypted(&db).is_ok());
    }

    #[test]
    fn recover_clears_stale_marker_on_plaintext() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        write_fake_sqlite(&db);
        write_marker(&db).expect("marker");

        recover_interrupted_migration(&db).expect("recover");
        assert!(!is_at_rest_encrypted(&db), "stale marker must be cleared");
        assert!(concerto_core::helpers::is_sqlite_file(&db), "plaintext file untouched");
    }

    #[test]
    fn recover_keeps_marker_on_encrypted_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        write_fake_encrypted(&db);
        write_marker(&db).expect("marker");

        recover_interrupted_migration(&db).expect("recover");
        assert!(is_at_rest_encrypted(&db), "healthy encrypted file keeps its marker");
        assert!(db.is_file());
    }

    #[test]
    fn recover_finishes_swap_when_main_file_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        write_fake_encrypted(&tmp_path(&db));
        write_fake_sqlite(&old_path(&db));
        write_marker(&db).expect("marker");

        recover_interrupted_migration(&db).expect("recover");
        assert!(db.is_file(), "swap must be completed from .enc-tmp");
        assert!(!tmp_path(&db).exists(), ".enc-tmp consumed");
        assert!(old_path(&db).exists(), ".old kept for the sweep");
        assert!(is_at_rest_encrypted(&db), "marker retained");
    }

    #[test]
    fn recover_restores_old_when_export_is_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        write_fake_sqlite(&old_path(&db));
        write_marker(&db).expect("marker");

        recover_interrupted_migration(&db).expect("recover");
        assert!(concerto_core::helpers::is_sqlite_file(&db), "plaintext original restored");
        assert!(!is_at_rest_encrypted(&db), "marker dropped with the rollback");
    }

    #[test]
    fn recover_drops_orphan_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        write_marker(&db).expect("marker");

        recover_interrupted_migration(&db).expect("recover");
        assert!(!is_at_rest_encrypted(&db));
        assert!(!db.exists(), "no database was conjured up");
    }

    #[test]
    fn recover_is_noop_without_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("sessions.db");
        write_fake_sqlite(&db);
        write_fake_sqlite(&tmp_path(&db));

        recover_interrupted_migration(&db).expect("recover");
        assert!(tmp_path(&db).exists(), "unrelated files are left alone");
    }

    #[test]
    fn audit_settings_defaults_keep_status_quo() {
        let settings = AuditSettings::from_config(AppConfig::default());
        assert!(!settings.encrypt_at_rest, "encryption stays opt-in");
        assert_eq!(settings.retention_days, None, "retention stays disabled");
        assert_eq!(settings.archive_dir, None);
    }

    #[test]
    fn audit_settings_fold_zero_retention_and_paths() {
        // `AuditConfig` lives in config's private `schema` module (reached in
        // practice through serde); `Default` + field access is enough here.
        let mut config = AppConfig { audit: Some(Default::default()), ..AppConfig::default() };
        if let Some(audit) = config.audit.as_mut() {
            audit.encrypt_at_rest = true;
            audit.retention_days = Some(0);
            audit.archive_dir = Some(camino::Utf8PathBuf::from("/var/audit"));
        }
        let settings = AuditSettings::from_config(config);
        assert!(settings.encrypt_at_rest);
        assert_eq!(settings.retention_days, None, "0 means disabled, not delete-all");
        assert_eq!(settings.archive_dir.as_deref(), Some(Path::new("/var/audit")));
    }
}
