//! ADR-65 §2 — workspace snapshot readiness barrier.
//!
//! Before planning begins, `run_multi_agent` captures a **deterministic,
//! language-agnostic project-tree inventory** — relative paths + size + mtime
//! (+ a blake3 content hash for files ≤ 64 KiB) — and makes it visible to
//! planning three ways:
//!
//! 1. the returned record's [`digest`](WorkspaceSnapshotRecord::digest) rides
//!    into every dispatched agent's context (during multi-agent dispatch, and
//!    injected into generic prompts as `<workspace_snapshot>`);
//! 2. a `WorkspaceSnapshot` whiteboard event is appended to the session log
//!    (the authoritative source, ADR-65 §1);
//! 3. the same observation is applied into the derived `resource_facts` store,
//!    so cached facts start clean.
//!
//! Planning waits on this barrier **only** — never on the asynchronously
//! spawned vector indexing. The capture is read-only (no language detection,
//! no writes) and everything fails soft: an unavailable pool, unreadable
//! directory, or an unapplyable snapshot degrades to a warning and the run
//! proceeds with whatever the digest path still provides.

use std::path::{Path, PathBuf};

use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use concerto_sessions::whiteboard::append_whiteboard_event;
use concerto_sessions::{
    NewWhiteboardEvent, ResourceFacts, SnapshotEntry, WhiteboardKind, WorkspaceSnapshotPayload,
};
use tracing::{debug, warn};

/// Directories and files excluded from the inventory at any depth.
///
/// Mirrors the memory indexer's default `exclude_patterns` (concerto-memory's
/// full `.gitignore`/`.concertoignore`-aware matcher is crate-private, so the
/// snapshot documents these defaults instead of reaching into concerto-memory
/// internals — same tree ⇒ same inventory in both systems for the defaults).
const DEFAULT_SKIP_COMPONENTS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    "coverage",
    "__pycache__",
    ".cache",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".venv",
    "venv",
    "vendor",
];

/// Content hashing is applied "where cheap" (ADR-65 §2): files at or below
/// this size are hashed; larger files record `content_hash: None` so the walk
/// stays bounded on huge trees.
/// Files ≤ this size get a content hash in the inventory — aliased to the
/// **single shared cache bound** (`concerto-sessions::resource_facts`), so the
/// walk, the derive, and the serve gate all agree on the hashing budget
/// (ADR-65 F2b).
const MAX_HASH_BYTES: u64 = concerto_sessions::resource_facts::CACHE_LIMIT_BYTES as u64;

/// Author attribution for snapshot events. The runtime — not a model — authors
/// `WorkspaceSnapshot` rows (ADR-65 §1); `coordinator` is the orchestrator
/// identity that runs the barrier, matching the convention used by other
/// coordinator-authored whiteboard events (e.g. `plan-approved`).
const BARRIER_AGENT_ID: &str = "coordinator";

/// The inventory captured at one point in time, before planning began, with
/// the deterministic generation id derived from it.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshotRecord {
    /// Content-addressed id (`blake3` over the sorted entries' metadata). An
    /// unchanged tree yields the same generation id; any recorded change
    /// (path, size, mtime, or content hash) yields a new one.
    pub generation: String,
    /// Sorted inventory entries (relative forward-slash paths).
    pub entries: Vec<SnapshotEntry>,
    /// Unix epoch milliseconds (UTC) at capture time.
    pub captured_at_ms: u64,
    /// The project root this snapshot was captured from — the identity that
    /// scopes every derived `resource_facts` row (ADR-65 F5c) and feeds the
    /// digest's freshness reconciliation (ADR-65 F3).
    pub project_root: camino::Utf8PathBuf,
}

impl WorkspaceSnapshotRecord {
    /// A compact, deterministic, agent-readable summary of the snapshot:
    /// generation id, file/byte totals, and the top-level tree, capped at 30
    /// lines so the injected digest stays cheap for the model and the log.
    pub fn digest(&self) -> String {
        const MAX_LINES: usize = 30;

        let mut totals: std::collections::HashMap<&str, (u64, u64)> =
            std::collections::HashMap::new();
        let mut bytes: u64 = 0;
        for entry in &self.entries {
            let size = entry.size_bytes.unwrap_or(0);
            bytes = bytes.saturating_add(size);
            let component = entry.path.split('/').next().unwrap_or("");
            let slot = totals.entry(component).or_insert((0_u64, 0_u64));
            slot.0 = slot.0.saturating_add(1);
            slot.1 = slot.1.saturating_add(size);
        }

        let mut lines = vec![format!(
            "workspace-snapshot generation={} files={} bytes={}",
            self.generation,
            self.entries.len(),
            bytes
        )];

        let mut components: Vec<&str> = totals.keys().copied().collect();
        components.sort_unstable();
        let shown = components.len().min(MAX_LINES);
        for component in components.iter().take(shown) {
            let (files, component_bytes) = totals.get(component).copied().unwrap_or((0, 0));
            lines.push(format!("  {component}/ {files} files {component_bytes} bytes"));
        }
        if components.len() > shown {
            lines.push(format!("  ... and {} more top-level entries", components.len() - shown));
        }

        lines.join("\n")
    }

    /// The typed payload for persistence/reconcile: the content-addressed
    /// string `generation` id, the inventory entries (ADR-65 §2), and the
    /// project root's hash so every derived row lands in the right per-root
    /// scope (ADR-65 F5c).
    pub fn as_payload(&self) -> WorkspaceSnapshotPayload {
        WorkspaceSnapshotPayload {
            generation: self.generation.clone(),
            files: self.entries.clone(),
            project_root_hash: crate::tool_facts::project_root_hash(
                self.project_root.as_std_path(),
            ),
        }
    }
}

/// Deterministic, content-addressed generation id: `blake3` over the sorted
/// entries' recorded metadata (path, size, mtime, content-hash presence and
/// value). Input order is irrelevant — the entries are sorted internally, so
/// an unchanged tree produces the same id regardless of walk order.
fn compute_generation(entries: &[SnapshotEntry]) -> String {
    let mut sorted: Vec<&SnapshotEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hasher = blake3::Hasher::new();
    for entry in sorted {
        hasher.update(entry.path.as_bytes());
        hasher.update(&entry.size_bytes.unwrap_or(0).to_be_bytes());
        hasher.update(&entry.mtime_ms.unwrap_or(0).to_be_bytes());
        match &entry.content_hash {
            Some(hash) => {
                hasher.update(&[1]);
                hasher.update(hash.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Deterministic, language-agnostic inventory walk over `project_dir`:
///
/// - files only (directories are never recorded as entries),
/// - forward-slash relative paths regardless of host OS,
/// - the documented [`DEFAULT_SKIP_COMPONENTS`] excluded at any depth,
/// - symlinks never followed,
/// - result sorted by relative path.
///
/// The walk is best-effort: unreadable directories drop their subtree with a
/// debug log; unreadable files are still recorded (with `content_hash: None`);
/// a cancelled token stops the walk and returns the partial inventory.
pub fn capture_workspace_snapshot(
    project_dir: &Path,
    cancel: &CancellationToken,
) -> Vec<SnapshotEntry> {
    let mut entries: Vec<SnapshotEntry> = Vec::new();

    // Iterative DFS with an explicit stack — no recursion, bounded memory even
    // on deep trees.
    let mut stack: Vec<PathBuf> = vec![project_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            break;
        }
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                debug!(
                    path = %dir.display(),
                    %error,
                    "workspace snapshot: unreadable directory subtree skipped"
                );
                continue;
            }
        };
        for entry in read_dir.flatten() {
            if cancel.is_cancelled() {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if dir_component_is_skipped(&path) {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                if let Some(snapshot) = snapshot_file_entry(project_dir, &path) {
                    entries.push(snapshot);
                }
            }
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// True when the immediate file-name component of `path` is on the default
/// skip list (applied to directories and files alike, matching the indexer's
/// component-style excludes; `.git` must never leak into planning context).
fn dir_component_is_skipped(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            let name = name.to_string_lossy();
            DEFAULT_SKIP_COMPONENTS.contains(&name.as_ref())
        })
        .unwrap_or(false)
}

/// One file's inventory entry: forward-slash relative path, byte size, mtime,
/// and a blake3 content hash when the file is ≤ [`MAX_HASH_BYTES`] (and
/// readable). Unreadable metadata drops the file from the inventory.
fn snapshot_file_entry(project_dir: &Path, path: &Path) -> Option<SnapshotEntry> {
    let relative = path.strip_prefix(project_dir).ok()?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    let metadata = std::fs::metadata(path).ok()?;
    let size_bytes = metadata.len();
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as u64);
    let content_hash = if size_bytes <= MAX_HASH_BYTES {
        match std::fs::read(path) {
            Ok(bytes) => Some(blake3::hash(&bytes).to_hex().to_string()),
            Err(error) => {
                debug!(
                    path = %relative,
                    %error,
                    "workspace snapshot: content hash skipped for unreadable file"
                );
                None
            }
        }
    } else {
        None
    };
    Some(SnapshotEntry { path: relative, size_bytes: Some(size_bytes), mtime_ms, content_hash })
}

/// ADR-65 §2 readiness barrier: capture the workspace inventory and make it
/// visible to planning.
///
/// - The returned record's digest is injected into agent context (never
///   persisted state needed by the log).
/// - A `WorkspaceSnapshot` event is appended to the whiteboard log — the
///   authoritative source of truth — and the observation is applied into the
///   derived `resource_facts` store, both best-effort when `pool` is `Some`.
/// - `None` (no pool, or an unreadable project dir) degrades gracefully:
///   planning still receives a digest where one could be produced; a missing
///   dir returns `None`.
///
/// Cancellation stops the capture walk; persistence checks the token at its
/// statement boundaries and aborts without surfacing an error.
pub async fn run_snapshot_barrier(
    pool: Option<&sqlx::SqlitePool>,
    project_dir: &Path,
    session_id: &str,
    cancel: &CancellationToken,
) -> Option<WorkspaceSnapshotRecord> {
    if !project_dir.is_dir() {
        warn!(
            path = %project_dir.display(),
            "workspace snapshot skipped: project directory is not a readable directory"
        );
        return None;
    }

    // The tree walk is potentially large — run it off the async executor and
    // fail soft if the blocking task panics.
    let entries = match tokio::task::spawn_blocking({
        let project_dir = project_dir.to_path_buf();
        let cancel = cancel.clone();
        move || capture_workspace_snapshot(&project_dir, &cancel)
    })
    .await
    {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, "workspace snapshot capture failed; run continues without a snapshot");
            return None;
        }
    };

    let record = WorkspaceSnapshotRecord {
        generation: compute_generation(&entries),
        entries,
        captured_at_ms: unix_ms() as u64,
        project_root: project_dir.to_string_lossy().into_owned().into(),
    };

    match pool {
        Some(pool) => persist_snapshot(pool, session_id, &record, cancel).await,
        None => warn!(
            "no whiteboard pool available — WorkspaceSnapshot event not persisted; \
             planning still receives the digest (ADR-65 §2 fail-soft)"
        ),
    }

    Some(record)
}

/// Best-effort persistence of the snapshot observation: append the
/// `WorkspaceSnapshot` event to the whiteboard log (the authoritative source,
/// ADR-65 §1/§2) then apply it into the derived `resource_facts` store. Each
/// step fails soft — the barrier never fails the run.
async fn persist_snapshot(
    pool: &sqlx::SqlitePool,
    session_id: &str,
    record: &WorkspaceSnapshotRecord,
    cancel: &CancellationToken,
) {
    let event = NewWhiteboardEvent {
        event_id: Ulid::new().to_string(),
        agent_id: BARRIER_AGENT_ID.to_owned(),
        kind: WhiteboardKind::WorkspaceSnapshot,
        scope: String::new(),
        session_id: Some(session_id.to_owned()),
        plan_id: None,
        causation: None,
        payload: serde_json::to_value(record.as_payload()).unwrap_or_default(),
        pre_image_hash: None,
        created_at: record.captured_at_ms as i64,
    };

    let stored = match append_whiteboard_event(pool, &event).await {
        Ok(stored) => stored,
        Err(error) => {
            warn!(
                %error,
                "failed to append WorkspaceSnapshot event; run continues without it (ADR-65 §2 fail-soft)"
            );
            return;
        }
    };

    let facts = ResourceFacts::new(pool.clone());
    if let Err(error) = facts
        .apply_snapshot(
            &stored.event_id,
            &event.agent_id,
            event.created_at,
            &record.as_payload(),
            cancel,
        )
        .await
    {
        warn!(
            %error,
            "failed to apply WorkspaceSnapshot to resource_facts; run continues (ADR-65 §2 fail-soft)"
        );
    }
}

/// Unix epoch milliseconds (UTC); `0` on a pre-epoch clock (never expected).
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use concerto_core::CancellationToken;
    use concerto_sessions::whiteboard::{load_whiteboard_events, WhiteboardLoadOpts};
    use concerto_sessions::WhiteboardKind;

    use super::*;

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    fn write(root: &Path, relative: &str, contents: &[u8]) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("relative parent exists"))
            .expect("create parent dirs");
        std::fs::write(&path, contents).expect("write file");
        path
    }

    /// A representative, language-agnostic tree exercising every capture rule.
    fn sample_tree(root: &Path) {
        write(root, "src/main.rs", b"fn main() {}\n");
        write(root, "src/error.rs", b"pub struct Error;\n");
        write(root, "app.js", b"console.log('hi');\n");
        write(root, "notes.txt", b"plain text\n");
        // Default-skip components: version control + build/module dirs.
        write(root, ".git/config", b"[core]\n");
        write(root, "target/debug/app", b"ELF");
        write(root, "node_modules/pkg/index.js", b"x\n");
        // Over the 64 KiB content-hash threshold.
        write(root, "assets/big.bin", &vec![b'a'; 70 * 1024]);
        // A symlink that must never be followed.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("src"), root.join("src-link"))
            .expect("create symlink");
    }

    #[test]
    fn capture_is_deterministic_and_language_agnostic() {
        let root = tempfile::tempdir().expect("tempdir created");
        sample_tree(root.path());
        let cancel = token();

        let entries = capture_workspace_snapshot(root.path(), &cancel);
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();

        assert_eq!(
            paths,
            vec!["app.js", "assets/big.bin", "notes.txt", "src/error.rs", "src/main.rs"],
            "forward-slash relative paths, files only, sorted"
        );
        assert!(
            !paths.iter().any(|p| {
                p.contains(".git/")
                    || p.contains("target/")
                    || p.contains("node_modules/")
                    || p.starts_with("src-link/")
            }),
            "skip list + symlinks keep their subtrees out of the inventory"
        );

        for entry in &entries {
            assert!(entry.size_bytes.is_some(), "size for {}", entry.path);
            assert!(entry.mtime_ms.is_some(), "mtime for {}", entry.path);
        }

        let main = entries.iter().find(|e| e.path == "src/main.rs").expect("src/main.rs present");
        assert!(main.content_hash.is_some(), "small file is content-hashed");

        let big = entries.iter().find(|e| e.path == "assets/big.bin").expect("big.bin present");
        assert_eq!(big.size_bytes, Some(70 * 1024));
        assert!(big.content_hash.is_none(), "large file is not content-hashed");

        // Determinism: a second, independent walk of the same tree is identical.
        let again = capture_workspace_snapshot(root.path(), &cancel);
        assert_eq!(entries, again);
    }

    #[test]
    fn generation_is_order_independent_and_content_addressed() {
        let first = vec![
            SnapshotEntry {
                path: "a.txt".to_owned(),
                size_bytes: Some(3),
                mtime_ms: Some(1),
                content_hash: Some("hash-a".to_owned()),
            },
            SnapshotEntry {
                path: "b.txt".to_owned(),
                size_bytes: Some(4),
                mtime_ms: Some(2),
                content_hash: None,
            },
        ];
        let mut shuffled = first.clone();
        shuffled.reverse();
        assert_eq!(
            compute_generation(&first),
            compute_generation(&shuffled),
            "generation id ignores inventory order"
        );

        let changed_mtime = vec![
            first[0].clone(),
            SnapshotEntry {
                path: "b.txt".to_owned(),
                size_bytes: Some(4),
                mtime_ms: Some(9),
                content_hash: None,
            },
        ];
        assert_ne!(
            compute_generation(&first),
            compute_generation(&changed_mtime),
            "recorded metadata change yields a new generation id"
        );
    }

    #[test]
    fn generation_tracks_workspace_content() {
        let root = tempfile::tempdir().expect("tempdir created");
        write(root.path(), "a.rs", b"let x = 1;\n");
        let cancel = token();

        let gen_a = compute_generation(&capture_workspace_snapshot(root.path(), &cancel));
        let gen_b = compute_generation(&capture_workspace_snapshot(root.path(), &cancel));
        assert_eq!(gen_a, gen_b, "unchanged tree keeps the same generation id");

        write(root.path(), "a.rs", b"let x = 12;\n");
        let gen_c = compute_generation(&capture_workspace_snapshot(root.path(), &cancel));
        assert_ne!(gen_a, gen_c, "changed content produces a new generation id");
    }

    #[test]
    fn digest_reports_totals_and_top_level_tree() {
        let record = WorkspaceSnapshotRecord {
            generation: "gen-1234".to_owned(),
            entries: vec![
                SnapshotEntry {
                    path: "src/a.rs".to_owned(),
                    size_bytes: Some(10),
                    mtime_ms: Some(1),
                    content_hash: None,
                },
                SnapshotEntry {
                    path: "src/b.rs".to_owned(),
                    size_bytes: Some(20),
                    mtime_ms: Some(2),
                    content_hash: None,
                },
                SnapshotEntry {
                    path: "docs/readme.md".to_owned(),
                    size_bytes: Some(100),
                    mtime_ms: Some(3),
                    content_hash: None,
                },
            ],
            captured_at_ms: 7,
            project_root: "/proj".into(),
        };

        let digest = record.digest();
        assert!(digest.contains("generation=gen-1234"), "digest carries the generation id");
        assert!(digest.contains("files=3"), "digest carries file total");
        assert!(digest.contains("bytes=130"), "digest carries byte total");
        assert!(digest.contains("src/ 2 files 30 bytes"), "digest aggregates per top-level dir");
        assert!(digest.contains("docs/ 1 files 100 bytes"));
    }

    #[test]
    fn digest_caps_at_thirty_lines() {
        let entries = (0..40)
            .map(|i| SnapshotEntry {
                path: format!("component-{i:02}/file.rs"),
                size_bytes: Some(i),
                mtime_ms: Some(1),
                content_hash: None,
            })
            .collect();
        let record = WorkspaceSnapshotRecord {
            generation: "gen".to_owned(),
            entries,
            captured_at_ms: 0,
            project_root: "/proj".into(),
        };

        let digest = record.digest();
        assert!(
            digest.lines().count() <= 32,
            "digest must stay bounded: {} lines\n{digest}",
            digest.lines().count()
        );
        assert!(digest.contains("files=40"), "digest reports the full total");
        assert!(digest.contains("and 10 more top-level entries"), "digest records the cap");
    }

    /// A hermetic SQLite pool with the sessions schema applied (same PRAGMAs
    /// and migration set the real store uses).
    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let db_path = dir.path().join("snapshot_test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations")
            .run(&pool)
            .await
            .expect("sessions migrations apply");
        (dir, pool)
    }

    #[tokio::test]
    async fn barrier_orders_snapshot_before_planning_and_applies_facts() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("tempdir created");
        write(root.path(), "src/main.rs", b"fn main() {}\n");
        let cancel = token();

        let record = run_snapshot_barrier(Some(&pool), root.path(), "session-1", &cancel)
            .await
            .expect("barrier produces a record for a readable project");
        assert!(!record.entries.is_empty(), "capture inventoried files");

        // Simulate the planner's first emission AFTER the barrier: a real
        // decompose run would emit TaskGraph/SubtaskStarted here, but the unit
        // test appends one representative planning-kind event to prove ordering.
        let planning = NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::TaskGraph,
            scope: String::new(),
            session_id: Some("session-1".to_owned()),
            plan_id: None,
            causation: Some(record.generation.clone()),
            payload: serde_json::json!({}),
            pre_image_hash: None,
            created_at: unix_ms(),
        };
        append_whiteboard_event(&pool, &planning).await.expect("planning event appended");

        let events = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: Some("session-1".to_owned()),
                scope: None,
                limit: 200,
            },
        )
        .await
        .expect("load whiteboard events");

        let snapshot = events
            .iter()
            .find(|event| event.kind == WhiteboardKind::WorkspaceSnapshot)
            .expect("workspace snapshot event present");
        let planning = events
            .iter()
            .find(|event| event.kind == WhiteboardKind::TaskGraph)
            .expect("planning event present");
        assert!(
            snapshot.gate_seq < planning.gate_seq,
            "snapshot must precede planning in the global event order"
        );

        // The log (source of truth) carries the content-addressed string
        // `generation` id under the typed `generation` key.
        let generation = snapshot
            .payload
            .get("generation")
            .and_then(serde_json::Value::as_str)
            .expect("generation present in payload");
        assert_eq!(generation, record.generation);

        // The derived store applied the observation: clean rows for listed files,
        // scoped under the snapshot's own project root (ADR-65 F5c).
        let facts = ResourceFacts::new(pool.clone());
        let root_hash = crate::tool_facts::project_root_hash(root.path());
        let row = facts
            .lookup(&root_hash, "src/main.rs", &cancel)
            .await
            .expect("lookup succeeds")
            .expect("src/main.rs was observed");
        assert!(!row.dirty, "snapshot observation brands the row clean");
        assert_eq!(
            row.content_hash.as_deref(),
            Some(blake3::hash(b"fn main() {}\n").to_hex().as_str()),
            "content hash round-trips through the derived store"
        );
    }

    #[tokio::test]
    async fn barrier_fails_soft_without_pool_and_for_missing_dir() {
        let root = tempfile::tempdir().expect("tempdir created");
        write(root.path(), "a.txt", b"hello");
        let cancel = token();

        // No pool: no persistence attempted, but planning still gets the
        // digest-carrying record (ADR-65 §2 fail-soft).
        let record = run_snapshot_barrier(None, root.path(), "session-2", &cancel)
            .await
            .expect("record returned without a pool");
        assert_eq!(record.entries.len(), 1);
        assert_eq!(record.entries[0].path, "a.txt");
        assert!(record.digest().contains("files=1"), "digest reflects the captured tree");

        // Missing/unreadable project dir: no record at all.
        let missing = root.path().join("does-not-exist");
        let none = run_snapshot_barrier(None, &missing, "session-2", &cancel).await;
        assert!(none.is_none(), "missing project dir yields no barrier record");
    }

    #[test]
    fn skip_list_is_documented_and_applied_by_component() {
        // `dir_component_is_skipped` inspects the immediate file-name
        // component; the walk applies it at EVERY level, so the skip list
        // holds at any depth (a nested `target/` is pruned identically).
        for component in DEFAULT_SKIP_COMPONENTS {
            assert!(
                dir_component_is_skipped(&Path::new("/project").join(component)),
                "{component} should be skipped by its own component name"
            );
        }
        assert!(
            dir_component_is_skipped(&Path::new("/project/src").join(".git")),
            "a nested .git is skipped when the walk reaches it"
        );
        assert!(!dir_component_is_skipped(Path::new("/project/src/lib.rs")));
        assert!(
            !dir_component_is_skipped(Path::new("/project/build.rs")),
            "a *file* named build.rs is not on the skip list"
        );
    }
}
