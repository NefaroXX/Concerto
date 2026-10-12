//! H08 — managed experiment workspaces, end-to-end through the real gate.
//!
//! Drives [`create_experiment`], [`review_experiment`], and
//! [`promote_experiment`] over tempdir hosts with the REAL `WriteGate` +
//! `FilesystemTool` (the harness shape mirrors `file_checkpoints.rs`):
//!
//! - non-git dirty baseline + untracked inclusion (live disk, never HEAD);
//! - git-worktree mode carries dirty + untracked via the overlay;
//! - experiment edits never touch the host;
//! - clean promotion applies (write/create/delete) and is idempotent;
//! - host conflicts refuse per path with three-hash records (converged and
//!   host-only drift proven alongside);
//! - generated files, caches, and symlinks stay out of claim in both
//!   directions;
//! - shell-escaped (absolute-path) host writes surface as drift, never as
//!   promotion input;
//! - stale reviews refuse with the host untouched;
//! - foreign ownership aborts before any write; policy denial aborts
//!   retriable (with the applied subset named);
//! - verification trees scope generations independently;
//! - abandon cleans scratch without touching the host.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::policy::AuditLog;
use concerto_core::types::{Condition, PolicyRule, SessionContext, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_orchestrator::experiment::{
    create_experiment, mark_abandoned, promote_experiment, remove_copy_workspace,
    remove_worktree_workspace, review_experiment, ExperimentError, ExperimentMode,
    ExperimentStatus, PromotionOutcome,
};
use concerto_orchestrator::gate::{FilePreImageReader, GateRequest, WriteGate};
use concerto_orchestrator::verify::{capture_scoped_generation, verify_root_for, VerifyTree};
use concerto_tools::filesystem::FilesystemTool;
use serde_json::json;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use tempfile::TempDir;

/// No-op audit log (mirrors the sibling suites).
struct TestAudit;

#[async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: concerto_core::traits::policy::AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        Ok(())
    }
}

fn allow_rules() -> Vec<PolicyRule> {
    vec![PolicyRule::AutoApprove(Condition::Always)]
}

async fn test_pool() -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let path = dir.path().join("exp_test.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);
    let pool = PoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

fn gate_for(
    pool: &sqlx::SqlitePool,
    root: &Path,
    rules: Vec<PolicyRule>,
) -> (Arc<ToolExecutor>, Arc<WriteGate>) {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.to_path_buf()).expect("tempdir is utf-8"),
    )));
    let engine = Arc::new(SimplePolicyEngine::new(rules, Arc::new(TestAudit)));
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), engine.clone()));
    let gate = Arc::new(WriteGate::new(
        engine,
        executor.clone(),
        pool.clone(),
        Arc::new(FilePreImageReader::new(root.to_path_buf())),
        root.to_path_buf(),
        1,
    ));
    (executor, gate)
}

/// One run's wiring: tempdir host, scratch sibling dir for workspaces, pool,
/// and an allow-all gate rooted at the host.
struct Harness {
    _host_dir: TempDir,
    _scratch_dir: TempDir,
    _db_dir: TempDir,
    root: PathBuf,
    pool: sqlx::SqlitePool,
    gate: Arc<WriteGate>,
    session_id: Ulid,
}

impl Harness {
    async fn new() -> Self {
        let host_dir = tempfile::tempdir().expect("host tempdir");
        let root = host_dir.path().to_path_buf();
        let scratch_dir = tempfile::tempdir().expect("scratch tempdir");
        let (_db_dir, pool) = test_pool().await;
        let (_, gate) = gate_for(&pool, &root, allow_rules());
        Self {
            _host_dir: host_dir,
            _scratch_dir: scratch_dir,
            _db_dir,
            root,
            pool,
            gate,
            session_id: Ulid::new(),
        }
    }

    /// Fresh workspace path that does not exist yet (creation refuses reuse).
    fn workspace(&self, name: &str) -> PathBuf {
        self._scratch_dir.path().join(format!("{name}-{}", Ulid::new()))
    }

    fn session(&self) -> SessionContext {
        SessionContext::new(self.session_id, self.root.clone())
    }

    fn seed(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("seed parents");
        }
        std::fs::write(&path, content).expect("seed write");
    }

    fn disk(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).expect("host read")
    }

    fn disk_exists(&self, rel: &str) -> bool {
        self.root.join(rel).exists()
    }

    /// Direct host-side write (simulates the user / a shell-escaped tool —
    /// outside the gate, unattributed, exactly what conflict detection
    /// must catch).
    fn host_write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("host parents");
        }
        std::fs::write(&path, content).expect("host write");
    }

    async fn gated_write(&self, call_id: &str, agent: &str, path: &str, content: &str) {
        let request = GateRequest {
            call_id: call_id.to_owned(),
            agent_id: agent.to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({"operation": "write", "path": path, "content": content}),
            registered_as: None,
            session_id: Some(self.session_id.to_string()),
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: std::collections::BTreeMap::new(),
            orchestrator_authority: false,
        };
        self.gate
            .submit(request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("gated write {call_id} must succeed: {error:?}"));
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }
}

fn ws_write(ws: &Path, rel: &str, content: &str) {
    let path = ws.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("ws parents");
    }
    std::fs::write(&path, content).expect("ws write");
}

fn ws_read(ws: &Path, rel: &str) -> String {
    std::fs::read_to_string(ws.join(rel)).expect("ws read")
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git(host: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(host)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// 1. Dirty baseline + untracked inclusion (copy mode, git HEAD is not the base)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dirty_baseline_and_untracked_inclusion_copy_mode() {
    if !git_available() {
        eprintln!("SKIP: git binary unavailable");
        return;
    }
    let h = Harness::new().await;
    git(&h.root, &["init"]);
    git(&h.root, &["config", "user.email", "test@example.com"]);
    git(&h.root, &["config", "user.name", "test"]);
    h.seed("tracked.txt", "v1-committed");
    h.seed("stable.txt", "stable");
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-m", "base"]);
    // Dirty the tracked file AFTER the commit; add a file git never sees.
    h.host_write("tracked.txt", "v2-dirty");
    h.host_write("untracked.txt", "never-added");

    let ws = h.workspace("dirty-copy");
    let record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    // The workspace holds live-disk bytes — the dirty v2, not HEAD's v1 —
    // and the untracked file git knows nothing about.
    assert_eq!(ws_read(&ws, "tracked.txt"), "v2-dirty");
    assert_eq!(ws_read(&ws, "untracked.txt"), "never-added");
    assert_eq!(ws_read(&ws, "stable.txt"), "stable");
    let paths: Vec<&str> =
        record.baseline.iter().map(|entry| entry.canonical_path.as_str()).collect();
    assert!(paths.contains(&"tracked.txt") && paths.contains(&"untracked.txt"));
    let tracked = record
        .baseline
        .iter()
        .find(|entry| entry.canonical_path == "tracked.txt")
        .expect("tracked baselined");
    assert_eq!(tracked.baseline_hash, blake3::hash(b"v2-dirty").to_hex().to_string());
    assert!(!record.baseline_generation.is_empty());
    assert_eq!(record.status, ExperimentStatus::Active);
}

// ---------------------------------------------------------------------------
// 2. Git-worktree mode carries dirty + untracked via the overlay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn git_worktree_mode_carries_dirty_and_untracked() {
    if !git_available() {
        eprintln!("SKIP: git binary unavailable");
        return;
    }
    let h = Harness::new().await;
    git(&h.root, &["init"]);
    git(&h.root, &["config", "user.email", "test@example.com"]);
    git(&h.root, &["config", "user.name", "test"]);
    h.seed("tracked.txt", "v1-committed");
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-m", "base"]);
    h.host_write("tracked.txt", "v2-dirty");
    h.host_write("untracked.txt", "never-added");

    let ws = h.workspace("dirty-worktree");
    let record = create_experiment(
        &h.root,
        &ws,
        ExperimentMode::GitWorktree,
        None,
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("worktree creation");

    // Overlay carried the live-disk state onto the HEAD worktree.
    assert_eq!(ws_read(&ws, "tracked.txt"), "v2-dirty");
    assert_eq!(ws_read(&ws, "untracked.txt"), "never-added");
    // The worktree control file is real plumbing, kept out of every byte path.
    assert!(ws.join(".git").is_file(), "worktree gitdir pointer exists");
    assert!(
        record.excluded.iter().any(|exclusion| exclusion.path.as_deref() == Some(".git")),
        ".git named in the honesty list"
    );

    // Abandon + remove unregisters the worktree and drops scratch; the host
    // (and its git metadata) is untouched.
    let mut record = record;
    assert!(mark_abandoned(&mut record));
    remove_worktree_workspace(&h.root, &ws, &Harness::cancel()).await.expect("worktree removal");
    assert!(!ws.exists(), "scratch gone");
    let list = std::process::Command::new("git")
        .arg("-C")
        .arg(&h.root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("worktree list");
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains(&ws.display().to_string()),
        "worktree unregistered"
    );
    assert_eq!(h.disk("tracked.txt"), "v2-dirty", "host untouched by abandon+remove");
}

// ---------------------------------------------------------------------------
// 3. Experiment edits never touch the host
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edits_in_experiment_do_not_touch_host() {
    let h = Harness::new().await;
    h.seed("a.txt", "v1");
    let ws = h.workspace("isolation");
    let record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    ws_write(&ws, "a.txt", "v2-experiment");
    ws_write(&ws, "new.txt", "fresh");
    assert_eq!(h.disk("a.txt"), "v1", "host write fenced");
    assert!(!h.disk_exists("new.txt"), "host create fenced");

    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    assert_eq!(review.changed, vec!["a.txt".to_string()]);
    assert_eq!(review.created, vec!["new.txt".to_string()]);
    assert!(review.deleted.is_empty());
}

// ---------------------------------------------------------------------------
// 4. Clean promotion applies write/create/delete and is idempotent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promotion_applies_clean_changes() {
    let h = Harness::new().await;
    h.seed("a.txt", "v1");
    h.seed("del.txt", "bye");
    h.seed("keep.txt", "same");
    let ws = h.workspace("promote-clean");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    ws_write(&ws, "a.txt", "v2");
    ws_write(&ws, "sub/new.txt", "fresh");
    std::fs::remove_file(ws.join("del.txt")).expect("experiment delete");

    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    assert_eq!(review.changed, vec!["a.txt".to_string()]);
    assert_eq!(review.created, vec!["sub/new.txt".to_string()]);
    assert_eq!(review.deleted, vec!["del.txt".to_string()]);

    match promote_experiment(
        &mut record,
        &review,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("promotion")
    {
        PromotionOutcome::Promoted { report } => {
            assert_eq!(report.promoted, vec!["a.txt".to_string()]);
            assert_eq!(report.recreated, vec!["sub/new.txt".to_string()]);
            assert_eq!(report.deleted, vec!["del.txt".to_string()]);
            assert!(report.conflicts.is_empty() && report.refused_oversized.is_empty());
        }
        other => panic!("expected Promoted, got {other:?}"),
    }
    assert_eq!(record.status, ExperimentStatus::Promoted);
    assert_eq!(h.disk("a.txt"), "v2");
    assert_eq!(h.disk("sub/new.txt"), "fresh");
    assert!(!h.disk_exists("del.txt"));
    assert_eq!(h.disk("keep.txt"), "same");

    // Second promotion is a recorded no-op carrying the first report.
    let review_again = review_experiment(&record, &Harness::cancel()).await;
    assert!(review_again.is_err(), "finished records refuse review");
    match promote_experiment(
        &mut record,
        &review,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("re-promotion")
    {
        PromotionOutcome::AlreadyPromoted { report } => {
            assert_eq!(report.promoted, vec!["a.txt".to_string()]);
        }
        other => panic!("expected AlreadyPromoted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. Conflicts refuse per path; converged + host-only drift surface honestly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promotion_refuses_host_conflict_and_records_effects() {
    let h = Harness::new().await;
    h.seed("conflict.txt", "base");
    h.seed("clean.txt", "base");
    h.seed("converge.txt", "base");
    h.seed("drift.txt", "base");
    let ws = h.workspace("promote-conflict");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    ws_write(&ws, "conflict.txt", "experiment-bytes");
    ws_write(&ws, "clean.txt", "experiment-bytes");
    ws_write(&ws, "converge.txt", "same-on-both");
    // Host drifts: conflicting edit, converged identical bytes, and drift on
    // a path the experiment never touched.
    h.host_write("conflict.txt", "host-bytes");
    h.host_write("converge.txt", "same-on-both");
    h.host_write("drift.txt", "host-only");

    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    match promote_experiment(
        &mut record,
        &review,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("promotion runs")
    {
        PromotionOutcome::Partial { report } => {
            assert_eq!(report.conflicts.len(), 1, "only the true conflict refuses");
            let conflict = &report.conflicts[0];
            assert_eq!(conflict.path, "conflict.txt");
            assert_eq!(
                conflict.baseline_hash.as_deref(),
                Some(blake3::hash(b"base").to_hex().to_string().as_str())
            );
            assert_eq!(
                conflict.host_hash.as_deref(),
                Some(blake3::hash(b"host-bytes").to_hex().to_string().as_str())
            );
            assert_eq!(
                conflict.experiment_hash.as_deref(),
                Some(blake3::hash(b"experiment-bytes").to_hex().to_string().as_str())
            );
            assert_eq!(report.promoted, vec!["clean.txt".to_string()]);
            assert_eq!(report.already_matching, vec!["converge.txt".to_string()]);
            assert_eq!(report.host_drift_untouched, vec!["drift.txt".to_string()]);
        }
        other => panic!("expected Partial, got {other:?}"),
    }
    // The refused path keeps HOST bytes (never silently overwritten); the
    // clean path applied; the record stays Active for the fix → promote loop.
    assert_eq!(h.disk("conflict.txt"), "host-bytes");
    assert_eq!(h.disk("clean.txt"), "experiment-bytes");
    assert_eq!(h.disk("converge.txt"), "same-on-both");
    assert_eq!(h.disk("drift.txt"), "host-only");
    assert_eq!(record.status, ExperimentStatus::Active);
    assert!(record.last_report.is_some(), "effects recorded");
}

// ---------------------------------------------------------------------------
// 6. Generated files, caches, and symlinks stay out of claim both ways
// ---------------------------------------------------------------------------

#[tokio::test]
async fn generated_files_caches_and_symlinks_excluded() {
    let h = Harness::new().await;
    h.seed("src/main.rs", "fn main() {}");
    h.seed("target/debug/app", "binary-ish");
    h.seed("node_modules/pkg/index.js", "module.exports = 1;");
    #[cfg(unix)]
    std::os::unix::fs::symlink(h.root.join("src/main.rs"), h.root.join("link.rs"))
        .expect("symlink seed");

    let ws = h.workspace("exclusions");
    let record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    assert!(!ws.join("target/debug/app").exists(), "cache not copied");
    assert!(!ws.join("node_modules/pkg/index.js").exists(), "cache not copied");
    #[cfg(unix)]
    assert!(!ws.join("link.rs").exists(), "symlink never copied");
    let baseline_paths: Vec<&str> =
        record.baseline.iter().map(|entry| entry.canonical_path.as_str()).collect();
    assert!(!baseline_paths.iter().any(|path| path.starts_with("target/")));
    assert!(!baseline_paths.iter().any(|path| path.starts_with("node_modules/")));
    assert!(record.excluded.iter().any(|exclusion| exclusion.path.as_deref() == Some("target")));
    assert!(record
        .excluded
        .iter()
        .any(|exclusion| exclusion.path.as_deref() == Some("node_modules")));
    #[cfg(unix)]
    assert!(
        record.excluded.iter().any(|exclusion| exclusion.path.as_deref() == Some("link.rs")),
        "symlink named in the honesty list"
    );

    // Build outputs appearing in the workspace AFTER creation stay
    // out-of-claim too: review ignores them, promotion never carries them.
    ws_write(&ws, "target/fresh.rlib", "fresh-build");
    ws_write(&ws, "src/main.rs", "fn main() { 1 }");
    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    assert!(!review.created.iter().any(|path| path.starts_with("target/")));
    let mut record = record;
    match promote_experiment(
        &mut record,
        &review,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("promotion")
    {
        PromotionOutcome::Promoted { .. } => {}
        other => panic!("expected Promoted, got {other:?}"),
    }
    assert!(!h.disk_exists("target/fresh.rlib"), "generated output never promoted back");
    assert_eq!(h.disk("src/main.rs"), "fn main() { 1 }");
}

// ---------------------------------------------------------------------------
// 7. Shell-escaped (absolute-path) host writes are drift, never promotion input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_escape_is_host_drift_not_promotion() {
    let h = Harness::new().await;
    h.seed("e.txt", "v1");
    let ws = h.workspace("shell-escape");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    // A shell tool with a host cwd / absolute path writes the HOST directly,
    // bypassing the workspace: the experiment never sees it.
    let absolute = h.root.join("e.txt");
    std::fs::write(&absolute, "v2-shell").expect("absolute host write");
    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    assert!(
        review.changed.is_empty() && review.created.is_empty() && review.deleted.is_empty(),
        "workspace-relative review sees no shell-escaped write: {review:?}"
    );
    match promote_experiment(
        &mut record,
        &review,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("promotion")
    {
        PromotionOutcome::Promoted { report } => {
            assert_eq!(report.host_drift_untouched, vec!["e.txt".to_string()]);
            assert!(report.promoted.is_empty());
        }
        other => panic!("expected Promoted (nothing to carry), got {other:?}"),
    }
    assert_eq!(h.disk("e.txt"), "v2-shell", "shell bytes stand; promotion wrote nothing");
}

// ---------------------------------------------------------------------------
// 8. Stale reviews refuse with the host untouched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promotion_requires_fresh_review() {
    let h = Harness::new().await;
    h.seed("a.txt", "v1");
    let ws = h.workspace("stale-review");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    ws_write(&ws, "a.txt", "v2-reviewed");
    let stale = review_experiment(&record, &Harness::cancel()).await.expect("review");
    // The experiment moves on AFTER the review: promotion must refuse the
    // ghost approval, not carry the unreviewed bytes.
    ws_write(&ws, "a.txt", "v3-unreviewed");
    match promote_experiment(
        &mut record,
        &stale,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::StaleReview { .. }) => {}
        other => panic!("expected StaleReview, got {other:?}"),
    }
    assert_eq!(h.disk("a.txt"), "v1", "host untouched by refused promotion");
    assert_eq!(record.status, ExperimentStatus::Active);
    assert!(record.last_report.is_none(), "refusals before the scan record nothing");

    // A fresh review promotes the current bytes.
    let fresh = review_experiment(&record, &Harness::cancel()).await.expect("re-review");
    match promote_experiment(
        &mut record,
        &fresh,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("fresh promotion")
    {
        PromotionOutcome::Promoted { .. } => {}
        other => panic!("expected Promoted, got {other:?}"),
    }
    assert_eq!(h.disk("a.txt"), "v3-unreviewed");
}

// ---------------------------------------------------------------------------
// 9. Foreign ownership aborts before ANY write (fail-closed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ownership_conflict_aborts_before_any_write() {
    let h = Harness::new().await;
    h.seed("owned.txt", "v1");
    h.seed("clean.txt", "v1");
    let ws = h.workspace("ownership-abort");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    ws_write(&ws, "owned.txt", "v2-experiment");
    ws_write(&ws, "clean.txt", "v2-experiment");
    // Sibling agent acquires the path with IDENTICAL bytes: host content is
    // clean (no drift conflict), but the owner is foreign.
    h.gated_write("own-1", "agent-b", "owned.txt", "v1").await;
    assert_eq!(h.gate.ownership_owner("owned.txt").as_deref(), Some("agent-b"));

    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    match promote_experiment(
        &mut record,
        &review,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::OwnershipConflict { conflicts, applied }) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "owned.txt");
            assert_eq!(conflicts[0].owner, "agent-b");
            assert!(applied.is_empty(), "pre-check abort applies nothing");
        }
        other => panic!("expected OwnershipConflict, got {other:?}"),
    }
    assert_eq!(h.disk("clean.txt"), "v1", "even the clean path is not applied");
    assert_eq!(record.status, ExperimentStatus::Active);
}

// ---------------------------------------------------------------------------
// 10. Policy denial aborts with the applied subset named; stays retriable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_denial_aborts_and_stays_retriable() {
    let h = Harness::new().await;
    h.seed("aaa_keep.txt", "v1");
    h.seed("zzz_no.txt", "v1");
    let ws = h.workspace("policy-abort");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");
    ws_write(&ws, "aaa_keep.txt", "v2");
    ws_write(&ws, "zzz_no.txt", "v2");

    // First-match-wins: the keep path approves, everything else denies.
    let (_, mixed_gate) = gate_for(
        &h.pool,
        &h.root,
        vec![
            PolicyRule::AutoApprove(Condition::PathGlob("aaa*".to_string())),
            PolicyRule::AutoDeny(Condition::Always),
        ],
    );
    let review = review_experiment(&record, &Harness::cancel()).await.expect("review");
    match promote_experiment(
        &mut record,
        &review,
        &mixed_gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::PolicyDenied { path, applied, .. }) => {
            assert_eq!(path, "zzz_no.txt");
            assert_eq!(
                applied,
                vec!["aaa_keep.txt".to_string()],
                "sorted order applies keep first"
            );
        }
        other => panic!("expected PolicyDenied, got {other:?}"),
    }
    assert_eq!(h.disk("aaa_keep.txt"), "v2");
    assert_eq!(h.disk("zzz_no.txt"), "v1", "denied path writes nothing");
    assert_eq!(record.status, ExperimentStatus::Active, "denial is retriable");

    // Retry under the allow gate (same promoter: self-owned paths pass the
    // pre-check) completes the promotion.
    let fresh = review_experiment(&record, &Harness::cancel()).await.expect("re-review");
    match promote_experiment(
        &mut record,
        &fresh,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    .expect("retry")
    {
        PromotionOutcome::Promoted { report } => {
            assert_eq!(report.promoted, vec!["zzz_no.txt".to_string()]);
            assert_eq!(report.already_matching, vec!["aaa_keep.txt".to_string()]);
        }
        other => panic!("expected Promoted on retry, got {other:?}"),
    }
    assert_eq!(h.disk("zzz_no.txt"), "v2");
}

// ---------------------------------------------------------------------------
// 11. Verification trees scope generations independently (H08 verify scope)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_tree_scope_is_independent() {
    use concerto_orchestrator::verify::VerifyTreeError;
    let h = Harness::new().await;
    h.seed("a.txt", "v1");
    let ws = h.workspace("verify-scope");
    let record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");
    let scope = VerifyTree::Experiment { experiment_id: record.experiment_id.clone() };

    assert_eq!(VerifyTree::Host.label(), "host");
    assert_eq!(scope.label(), format!("experiment:{}", record.experiment_id));
    assert!(!VerifyTree::Host.is_experiment() && scope.is_experiment());

    let (host_root, host_gen) =
        capture_scoped_generation(&h.root, &VerifyTree::Host, None, &Harness::cancel())
            .expect("host generation");
    assert_eq!(host_root, h.root);
    let (exp_root, exp_gen) =
        capture_scoped_generation(&h.root, &scope, Some(&ws), &Harness::cancel())
            .expect("experiment generation");
    assert_eq!(exp_root, ws);

    // An experiment edit moves the experiment generation only — the host
    // generation (and the host freshness clock armed from it) is untouched.
    ws_write(&ws, "a.txt", "v2-experiment");
    let (_, host_gen_after) =
        capture_scoped_generation(&h.root, &VerifyTree::Host, None, &Harness::cancel())
            .expect("host generation again");
    let (_, exp_gen_after) =
        capture_scoped_generation(&h.root, &scope, Some(&ws), &Harness::cancel())
            .expect("experiment generation again");
    assert_eq!(host_gen_after, host_gen, "host generation stable under experiment edits");
    assert_ne!(exp_gen_after, exp_gen, "experiment generation binds its own bytes");

    // No silent host fallback: an experiment scope without its root refuses.
    match verify_root_for(&h.root, &scope, None) {
        Err(VerifyTreeError::ExperimentRootMissing { .. }) => {}
        other => panic!("expected ExperimentRootMissing, got {other:?}"),
    }
    assert_eq!(verify_root_for(&h.root, &VerifyTree::Host, None).expect("host root"), h.root);
}

// ---------------------------------------------------------------------------
// 12. Roots validation + abandonment never touches the host
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creation_refuses_bad_roots_and_non_git_worktrees() {
    let h = Harness::new().await;
    h.seed("a.txt", "v1");

    // Existing workspace dir: refusing to reuse or merge.
    let existing = h.workspace("exists");
    std::fs::create_dir_all(&existing).expect("pre-create");
    match create_experiment(
        &h.root,
        &existing,
        ExperimentMode::Copy,
        None,
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::Invalid(_)) => {}
        other => panic!("expected Invalid for existing dir, got {other:?}"),
    }
    // Nested workspace under host.
    match create_experiment(
        &h.root.join("nested-ws"),
        &h.root,
        ExperimentMode::Copy,
        None,
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::Invalid(_)) => {}
        other => panic!("expected Invalid for nested roots, got {other:?}"),
    }
    // Worktree mode on a non-git host fails closed (no silent copy fallback).
    let ws = h.workspace("not-a-repo");
    match create_experiment(
        &h.root,
        &ws,
        ExperimentMode::GitWorktree,
        None,
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::Git(_)) => {}
        other => panic!("expected Git failure on a non-repo, got {other:?}"),
    }
}

#[tokio::test]
async fn abandon_never_touches_host() {
    let h = Harness::new().await;
    h.seed("a.txt", "v1");
    let ws = h.workspace("abandon");
    let mut record =
        create_experiment(&h.root, &ws, ExperimentMode::Copy, None, "agent-a", &Harness::cancel())
            .await
            .expect("creation");

    ws_write(&ws, "a.txt", "v2-experiment");
    assert!(mark_abandoned(&mut record), "first abandon flips");
    assert!(!mark_abandoned(&mut record), "second abandon is a no-op");
    remove_copy_workspace(&ws).await.expect("scratch removal");
    assert!(!ws.exists());
    assert_eq!(h.disk("a.txt"), "v1", "abandon + removal never touches the host");

    // Finished records refuse both review and promotion.
    assert!(review_experiment(&record, &Harness::cancel()).await.is_err());
    let stale_like = concerto_orchestrator::experiment::ExperimentDiff {
        experiment_id: record.experiment_id.clone(),
        baseline_digest: record.baseline_digest(),
        experiment_digest: String::new(),
        changed: Vec::new(),
        created: Vec::new(),
        deleted: Vec::new(),
        unchanged: 0,
        oversized: Vec::new(),
    };
    match promote_experiment(
        &mut record,
        &stale_like,
        &h.gate,
        &h.session(),
        "agent-a",
        &Harness::cancel(),
    )
    .await
    {
        Err(ExperimentError::AlreadyFinished { .. }) => {}
        other => panic!("expected AlreadyFinished, got {other:?}"),
    }
}
