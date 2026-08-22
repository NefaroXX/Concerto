//! Filesystem watcher for Concerto config files (ADR-57).
//!
//! Watches the global config dir(s) and the active project dir for edits to
//! Concerto config files so external changes propagate to the next agent run
//! without a restart. Mirrors the memory-crate watcher shape
//! (`crates/memory/src/watcher.rs`): `notify` + `notify-debouncer-mini` with a
//! 1s debounce, and a debouncer callback that runs on its own std thread and
//! must never block or await (try_send only, drop-on-overflow is intentional).
//!
//! Events are filtered by exact file name, so unrelated files sharing a config
//! dir never produce false reloads. The app reload path re-reads the files and
//! equality-short-circuits unchanged content, so a single coalesced "something
//! changed" signal is all this watcher needs to emit.

use std::path::{Path, PathBuf};
use std::time::Duration;

use concerto_core::CancellationToken;
use notify::{RecommendedWatcher, RecursiveMode};
use tokio::sync::mpsc;

/// File names whose changes are forwarded. `config.toml` covers both the new
/// and legacy global dirs (they share the file name); the project-scoped names
/// differ by era. The ADR-58 blueprint include file is tracked too, so edits
/// to `orchestration.blueprint.toml` propagate to the next run exactly like
/// `config.toml` edits do (the reload path re-reads it at load time).
const TRACKED_NAMES: [&str; 4] = [
    "config.toml",
    concerto_config::legacy::NEW_PROJECT_CONFIG_FILE,
    concerto_config::legacy::OLD_PROJECT_CONFIG_FILE,
    concerto_config::blueprint::BLUEPRINT_INCLUDE_FILE,
];

/// Whether `path` names one of the tracked config files (exact-name match).
fn is_tracked(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| TRACKED_NAMES.iter().any(|tracked| tracked == &name))
        .unwrap_or(false)
}

/// Compute the directory paths the watcher observes: the new global config
/// dir (created on demand so file creation for a fresh install is observed),
/// the legacy global config dir (watched only while it actually exists — all
/// writes use the new path), and the active project dir.
fn watched_dirs(project_dir: PathBuf) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(config_dir) = dirs::config_dir() {
        let new_dir = config_dir.join(concerto_config::legacy::NEW_CONFIG_DIR);
        if std::fs::create_dir_all(&new_dir).is_ok() {
            paths.push(new_dir);
        } else {
            tracing::warn!(
                dir = %new_dir.display(),
                "failed to create config directory; it will not be watched"
            );
        }
        let legacy_dir = config_dir.join(concerto_config::legacy::OLD_CONFIG_DIR);
        if legacy_dir.exists() {
            paths.push(legacy_dir);
        }
    }
    paths.push(project_dir);
    paths
}

/// Active config watcher and its event stream. Owning this value keeps the
/// native `notify` watcher (inside the debouncer) alive; dropping it stops
/// observation immediately.
pub struct ConfigWatch {
    receiver: mpsc::Receiver<()>,
    _watcher: Option<notify_debouncer_mini::Debouncer<RecommendedWatcher>>,
    cancel: CancellationToken,
}

impl ConfigWatch {
    /// Start watching `project_dir` plus the global config dir(s).
    ///
    /// Infallible by design: a transient notify failure degrades to "no
    /// watcher" (logged once) rather than breaking the whole subscription.
    pub fn start(project_dir: PathBuf) -> Self {
        Self::start_paths(watched_dirs(project_dir))
    }

    /// Start watching an explicit list of directories. Kept private so tests
    /// can inject tempdirs.
    fn start_paths(paths: Vec<PathBuf>) -> Self {
        let (tx, receiver) = mpsc::channel(100);

        let mut debouncer = match notify_debouncer_mini::new_debouncer(
            Duration::from_secs(1),
            move |res: notify_debouncer_mini::DebounceEventResult| match res {
                Ok(events) => {
                    // Exact-name filter neutralizes event noise from other
                    // files sharing the config dir. One coalesced signal is
                    // enough: the reload path re-reads the files and
                    // equality-short-circuits unchanged ones. Dropping on
                    // overflow is intentional — a busy app simply gets the
                    // next debounce batch. Never block on a std thread.
                    if events.iter().any(|event| is_tracked(&event.path)) {
                        let _ = tx.try_send(());
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "config watcher debounce reported an error; continuing");
                }
            },
        ) {
            Ok(debouncer) => debouncer,
            Err(error) => {
                tracing::error!(
                    %error,
                    "failed to create config watcher; external config edits will not propagate"
                );
                return Self { receiver, _watcher: None, cancel: CancellationToken::new() };
            }
        };

        for dir in &paths {
            if let Err(error) = debouncer.watcher().watch(dir, RecursiveMode::NonRecursive) {
                tracing::warn!(%error, dir = %dir.display(), "failed to watch config directory");
            }
        }

        Self { receiver, _watcher: Some(debouncer), cancel: CancellationToken::new() }
    }

    /// Await the next coalesced "config files changed" signal. Returns `None`
    /// once cancelled or the channel closes.
    pub async fn recv(&mut self) -> Option<()> {
        tokio::select! {
            _ = self.cancel.cancelled() => None,
            received = self.receiver.recv() => received,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_names_cover_every_config_filename() {
        // The ADR-58 blueprint include file is tracked alongside config.toml:
        // an edit must also propagate to the next run.
        assert_eq!(TRACKED_NAMES[3], concerto_config::blueprint::BLUEPRINT_INCLUDE_FILE);
        for name in TRACKED_NAMES {
            assert!(is_tracked(&PathBuf::from(format!("/some/dir/{name}"))), "{name}");
        }
        assert!(!is_tracked(&PathBuf::from("/some/dir/other.toml")));
        assert!(!is_tracked(&PathBuf::from("/some/dir/notes.md")));
        assert!(!is_tracked(&PathBuf::from("/some/dir")));
    }

    #[tokio::test]
    async fn write_to_watched_config_file_produces_signal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_owned();
        let file = dir.join("config.toml");
        std::fs::write(&file, "key = \"one\"\n").expect("seed config");
        let mut watch = ConfigWatch::start_paths(vec![dir]);

        std::fs::write(&file, "key = \"two\"\n").expect("rewrite config");

        // A signal is the unit payload by design — the reload path re-reads
        // the files itself.
        tokio::time::timeout(std::time::Duration::from_secs(5), watch.recv())
            .await
            .expect("timed out waiting for config change signal")
            .expect("watcher closed unexpectedly");
    }

    #[tokio::test]
    async fn unrelated_file_in_watched_dir_is_ignored() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_owned();
        let watch = ConfigWatch::start_paths(vec![dir.clone()]);

        let unrelated = dir.join("scratch.log");
        std::fs::write(&unrelated, "noise\n").expect("write unrelated file");

        // Debounce window is 1s; give the ignored write time that it must NOT
        // surface as a signal.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            watch.receiver.is_empty(),
            "unrelated files must not produce a config-reload signal"
        );
    }

    #[tokio::test]
    async fn cancel_stops_recv() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut watch = ConfigWatch::start_paths(vec![temp.path().to_owned()]);
        watch.cancel.cancel();
        let value = tokio::time::timeout(std::time::Duration::from_secs(5), watch.recv())
            .await
            .expect("recv did not return after cancel");
        assert!(value.is_none());
    }
}
