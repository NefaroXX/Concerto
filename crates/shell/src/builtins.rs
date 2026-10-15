use std::ffi::OsStr;
use std::sync::Arc;

use async_trait::async_trait;
use camino::Utf8Path;
use concerto_core::CancellationToken;
use ignore::DirEntry;
use serde::Serialize;
use serde_json::json;

use crate::path::{canonicalize_root, resolve_in_canonical_root, PathError};
use crate::{
    CommandEffect, CommandInvocation, CommandProvenance, CommandResult, CommandServices,
    CommandSource, CommandSpec, CommandStatus, Diagnostic, ShellCommand, ShellContext,
};

const MAX_TREE_DEPTH: usize = 64;
const DEFAULT_TREE_DEPTH: usize = 4;
const MAX_TREE_ENTRIES: usize = 10_000;

pub(crate) fn standard_commands() -> Vec<Arc<dyn ShellCommand>> {
    vec![
        Arc::new(HelpCommand::new()),
        Arc::new(ProjectInfoCommand::new()),
        Arc::new(TreeCommand::new()),
        Arc::new(LastCommand::new()),
    ]
}

struct HelpCommand {
    spec: CommandSpec,
}

impl HelpCommand {
    fn new() -> Self {
        Self {
            spec: CommandSpec {
                name: "help".to_owned(),
                description: "List commands or describe one command".to_owned(),
                usage: "help [command]".to_owned(),
                source: CommandSource::Builtin,
                effects: Vec::new(),
                records_history: false,
            },
        }
    }
}

#[async_trait]
impl ShellCommand for HelpCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        _context: &ShellContext,
        services: &CommandServices,
        _cancel: CancellationToken,
    ) -> CommandResult {
        if invocation.arguments.len() > 1 {
            return usage_failure(&self.spec, "help accepts at most one command name");
        }

        let specs = match services.command_specs() {
            Ok(specs) => specs,
            Err(error) => {
                return CommandResult::terminal(
                    &self.spec.name,
                    "shell.registry.unavailable",
                    error.to_string(),
                );
            }
        };

        if let Some(name) = invocation.arguments.first() {
            return match specs.into_iter().find(|spec| spec.name == *name) {
                Some(spec) => CommandResult::new(
                    &self.spec.name,
                    CommandStatus::Succeeded,
                    format!("{} — {}", spec.usage, spec.description),
                )
                .with_data(json!(spec)),
                None => CommandResult::recoverable(
                    &self.spec.name,
                    "shell.command.unknown",
                    format!("no command named `{name}` is registered"),
                )
                .with_suggestion("Run `help` to list available commands."),
            };
        }

        let summary = specs
            .iter()
            .map(|spec| format!("{:<16} {}", spec.name, spec.description))
            .collect::<Vec<_>>()
            .join("\n");
        CommandResult::new(&self.spec.name, CommandStatus::Succeeded, summary)
            .with_data(json!({ "commands": specs }))
    }
}

struct ProjectInfoCommand {
    spec: CommandSpec,
}

impl ProjectInfoCommand {
    fn new() -> Self {
        Self {
            spec: CommandSpec {
                name: "project-info".to_owned(),
                description: "Show the explicit Concerto project context".to_owned(),
                usage: "project-info".to_owned(),
                source: CommandSource::Builtin,
                effects: vec![CommandEffect::ProjectRead],
                records_history: true,
            },
        }
    }
}

#[async_trait]
impl ShellCommand for ProjectInfoCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        context: &ShellContext,
        _services: &CommandServices,
        cancel: CancellationToken,
    ) -> CommandResult {
        if !invocation.arguments.is_empty() {
            return usage_failure(&self.spec, "project-info does not accept arguments");
        }
        if cancel.is_cancelled() {
            return cancelled(&self.spec.name);
        }

        let project_name = context.project_root.file_name().unwrap_or("/");
        let cargo_manifest = context.project_root.join("Cargo.toml").is_file();
        let git_repository = context.project_root.join(".git").exists();
        let summary =
            format!("Project {project_name} at {} (cwd: {})", context.project_root, context.cwd);

        CommandResult::new(&self.spec.name, CommandStatus::Succeeded, summary).with_data(json!({
            "name": project_name,
            "project_root": context.project_root,
            "cwd": context.cwd,
            "project_id": context.project_id,
            "session_id": context.session_id,
            "provider": context.provider,
            "model": context.model,
            "agent_roles": context.agent_roles,
            "branch": context.branch,
            "cargo_manifest": cargo_manifest,
            "git_repository": git_repository,
        }))
    }
}

struct TreeCommand {
    spec: CommandSpec,
}

impl TreeCommand {
    fn new() -> Self {
        Self {
            spec: CommandSpec {
                name: "ls-tree".to_owned(),
                description: "List a project tree as bounded structured data".to_owned(),
                usage: "ls-tree [path] [--depth N]".to_owned(),
                source: CommandSource::Builtin,
                effects: vec![CommandEffect::ProjectRead],
                records_history: true,
            },
        }
    }
}

#[derive(Debug)]
struct TreeOptions {
    path: Option<String>,
    depth: usize,
}

#[derive(Debug, Serialize)]
struct TreeEntry {
    path: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
}

#[async_trait]
impl ShellCommand for TreeCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        context: &ShellContext,
        _services: &CommandServices,
        cancel: CancellationToken,
    ) -> CommandResult {
        let options = match parse_tree_options(&invocation.arguments) {
            Ok(options) => options,
            Err(message) => return usage_failure(&self.spec, &message),
        };

        if cancel.is_cancelled() {
            return cancelled(&self.spec.name);
        }

        let root = match canonicalize_root(&context.project_root) {
            Ok(root) => root,
            Err(error) => {
                return CommandResult::recoverable(
                    &self.spec.name,
                    "shell.project.unavailable",
                    format!("cannot access project root `{}`: {error}", context.project_root),
                );
            }
        };
        let requested = options.path.as_deref().unwrap_or(".");
        let target = match resolve_in_canonical_root(
            &root,
            &context.project_root,
            &context.cwd,
            requested,
        ) {
            Ok(target) => target,
            Err(error) => return tree_path_error(&self.spec.name, requested, error),
        };

        let (entries, diagnostics) =
            match collect_tree_entries(&root, &target, options.depth, MAX_TREE_ENTRIES, &cancel) {
                TreeWalk::Collected { entries, diagnostics } => (entries, diagnostics),
                TreeWalk::Cancelled => return cancelled(&self.spec.name),
            };

        let status = if diagnostics.is_empty() {
            CommandStatus::Succeeded
        } else {
            CommandStatus::SucceededWithWarnings
        };
        let summary = format!("Listed {} entries beneath {target}", entries.len());
        let mut result = CommandResult::new(&self.spec.name, status, summary).with_data(json!({
            "project_root": root,
            "target": target,
            "depth": options.depth,
            "entries": entries,
        }));
        result.diagnostics = diagnostics;
        result
    }
}

struct LastCommand {
    spec: CommandSpec,
}

impl LastCommand {
    fn new() -> Self {
        Self {
            spec: CommandSpec {
                name: "last".to_owned(),
                description: "Show the latest recorded command result".to_owned(),
                usage: "last [--json]".to_owned(),
                source: CommandSource::Builtin,
                effects: Vec::new(),
                records_history: false,
            },
        }
    }
}

#[async_trait]
impl ShellCommand for LastCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        _context: &ShellContext,
        services: &CommandServices,
        _cancel: CancellationToken,
    ) -> CommandResult {
        let as_json = match invocation.arguments.as_slice() {
            [] => false,
            [argument] if argument == "--json" => true,
            _ => return usage_failure(&self.spec, "last accepts only the optional --json flag"),
        };

        let previous = match services.last_result() {
            Ok(Some(previous)) => previous,
            Ok(None) => {
                return CommandResult::recoverable(
                    &self.spec.name,
                    "shell.history.empty",
                    "no command result has been recorded yet",
                );
            }
            Err(error) => {
                return CommandResult::terminal(
                    &self.spec.name,
                    "shell.history.unavailable",
                    error.to_string(),
                );
            }
        };

        let summary = if as_json {
            match previous.to_pretty_json() {
                Ok(json) => json,
                Err(error) => {
                    return CommandResult::terminal(
                        &self.spec.name,
                        "shell.result.serialization-failed",
                        error.to_string(),
                    );
                }
            }
        } else {
            format!("{}: {}", previous.command, previous.summary)
        };

        CommandResult::new(&self.spec.name, CommandStatus::Succeeded, summary)
            .with_data(json!({ "result": previous }))
    }
}

fn usage_failure(spec: &CommandSpec, message: &str) -> CommandResult {
    CommandResult::recoverable(&spec.name, "shell.arguments.invalid", message)
        .with_suggestion(format!("Usage: {}", spec.usage))
}

fn cancelled(command: &str) -> CommandResult {
    CommandResult::new(command, CommandStatus::Cancelled, "command cancelled by caller")
}

fn parse_tree_options(arguments: &[String]) -> Result<TreeOptions, String> {
    let mut path = None;
    let mut depth = DEFAULT_TREE_DEPTH;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--depth" {
            index += 1;
            let value =
                arguments.get(index).ok_or_else(|| "--depth requires a value".to_owned())?;
            depth = parse_depth(value)?;
        } else if let Some(value) = argument.strip_prefix("--depth=") {
            depth = parse_depth(value)?;
        } else if argument.starts_with('-') {
            return Err(format!("unknown option `{argument}`"));
        } else if path.replace(argument.clone()).is_some() {
            return Err("ls-tree accepts at most one path".to_owned());
        }
        index += 1;
    }

    Ok(TreeOptions { path, depth })
}

fn parse_depth(value: &str) -> Result<usize, String> {
    let depth = value.parse::<usize>().map_err(|_| format!("invalid depth `{value}`"))?;
    if depth > MAX_TREE_DEPTH {
        return Err(format!("depth must not exceed {MAX_TREE_DEPTH}"));
    }
    Ok(depth)
}

/// Map a root-confined resolution failure onto a `CommandResult` for
/// path-reading builtins: escapes block, I/O failures are recoverable.
fn tree_path_error(command: &str, requested: &str, error: PathError) -> CommandResult {
    match error {
        PathError::OutsideRoot { root, .. } | PathError::SymlinkEscape { root, .. } => {
            CommandResult::new(
                command,
                CommandStatus::Blocked,
                format!("path `{requested}` is outside project root `{root}`"),
            )
            .with_data(json!({ "requested_path": requested, "project_root": root }))
            .with_suggestion("Choose a path inside the active project.")
        }
        PathError::RootInaccessible { root, source } => CommandResult::recoverable(
            command,
            "shell.project.unavailable",
            format!("cannot access project root `{root}`: {source}"),
        ),
        other => CommandResult::recoverable(
            command,
            "shell.path.unavailable",
            format!("cannot access `{requested}`: {other}"),
        ),
    }
}

/// Outcome of walking a requested tree.
enum TreeWalk {
    /// Entries collected (possibly truncated) plus any per-entry warnings.
    Collected { entries: Vec<TreeEntry>, diagnostics: Vec<Diagnostic> },
    /// Cancellation was observed between entries.
    Cancelled,
}

/// Walk `target` (contained in `root`) with `.gitignore` filtering applied.
///
/// `ignore::WalkBuilder` replaces the previous `walkdir` walker so a built
/// `target/` (which can hold roughly ten times the files of the project itself)
/// cannot exhaust `max_entries` on Cargo build artifacts. The configuration
/// preserves the previous walkdir semantics and fixes the documented gap:
///
/// - `follow_links(false)`: symlinks are listed but never followed, so the walk
///   cannot leave `target` and bypass the root-containment gate in `execute`.
/// - `hidden(false)`: dotfiles are still listed, exactly as walkdir did;
///   hiding them would be an unrelated behaviour change.
/// - `sort_by_file_name(OsStr::cmp)`: deterministic per-directory ordering,
///   matching walkdir's `sort_by_file_name`.
/// - `git_global(false)` / `git_exclude(false)` / `parents(false)`: keep the
///   result deterministic and rooted at `target`. This is a policy-gated tool,
///   so it must not depend on the developer's personal global git config, on
///   `.git/info/exclude`, or on ignore rules from above `target`.
/// - `ignore(false)`: ripgrep-style `.ignore` files are conventionally
///   untracked, so honouring them would let the tool's output change based on a
///   file invisible to `git status`/`git diff` review — the same hidden-source
///   non-determinism the settings above exist to prevent.
/// - `require_git(false)`: load-bearing — `ignore` defaults this to `true`,
///   which disables `.gitignore` entirely outside a git repo and would leave
///   plain directories unfiltered.
///
/// The walk stays sequential (`build`, not `build_parallel`) so ordering is
/// deterministic and each yielded item can be checked against `cancel`.
fn collect_tree_entries(
    root: &Utf8Path,
    target: &Utf8Path,
    depth: usize,
    max_entries: usize,
    cancel: &CancellationToken,
) -> TreeWalk {
    let mut entries = Vec::new();
    let mut diagnostics = Vec::new();
    let walker = ignore::WalkBuilder::new(target)
        .follow_links(false)
        .max_depth(Some(depth))
        .sort_by_file_name(OsStr::cmp)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .build();

    for item in walker {
        if cancel.is_cancelled() {
            return TreeWalk::Cancelled;
        }
        if entries.len() == max_entries {
            diagnostics.push(Diagnostic::warning(
                "shell.tree.truncated",
                format!("tree output was limited to {max_entries} entries"),
            ));
            break;
        }

        match item {
            Ok(entry) => entries.push(tree_entry(root, &entry)),
            Err(error) => diagnostics
                .push(Diagnostic::warning("shell.tree.entry-unavailable", error.to_string())),
        }
    }

    TreeWalk::Collected { entries, diagnostics }
}

fn tree_entry(root: &Utf8Path, entry: &DirEntry) -> TreeEntry {
    let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
    let file_type = entry.file_type();
    let kind = match file_type {
        Some(file_type) => {
            if file_type.is_dir() {
                "directory"
            } else if file_type.is_file() {
                "file"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "other"
            }
        }
        None => "other",
    };
    let size = match file_type {
        Some(file_type) if file_type.is_file() => {
            entry.metadata().ok().map(|metadata| metadata.len())
        }
        _ => None,
    };

    TreeEntry {
        path: if relative.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            relative.to_string_lossy().into_owned()
        },
        kind,
        size,
    }
}

pub(crate) fn apply_provenance(
    result: &mut CommandResult,
    source: CommandSource,
    context: &ShellContext,
) {
    result.provenance = CommandProvenance {
        source,
        provider: context.provider.clone(),
        model: context.model.clone(),
        agent_role: None,
    };
}

#[cfg(test)]
mod tests {
    use camino::{Utf8Path, Utf8PathBuf};
    use concerto_core::CancellationToken;

    use super::{
        collect_tree_entries, parse_tree_options, tree_path_error, TreeCommand, TreeWalk,
        DEFAULT_TREE_DEPTH, MAX_TREE_DEPTH,
    };
    use crate::path::PathError;
    use crate::{
        CommandInvocation, CommandRegistry, CommandResult, CommandServices, CommandStatus,
        ShellCommand, ShellContext, ShellHistory,
    };

    #[test]
    fn parses_tree_options_in_either_order() {
        let options = parse_tree_options(&["--depth".to_owned(), "2".to_owned(), "src".to_owned()])
            .expect("valid options");
        assert_eq!(options.depth, 2);
        assert_eq!(options.path.as_deref(), Some("src"));

        let defaults = parse_tree_options(&[]).expect("valid defaults");
        assert_eq!(defaults.depth, DEFAULT_TREE_DEPTH);
    }

    #[test]
    fn rejects_excessive_depth() {
        let error = parse_tree_options(&[format!("--depth={}", MAX_TREE_DEPTH + 1)])
            .expect_err("depth should be bounded");
        assert!(error.contains("must not exceed"));
    }

    #[test]
    fn tree_path_error_maps_escape_to_blocked() {
        let result = tree_path_error(
            "ls-tree",
            "../escape",
            PathError::OutsideRoot {
                requested: Utf8PathBuf::from("../escape"),
                root: Utf8PathBuf::from("/project"),
            },
        );
        assert_eq!(result.status, CommandStatus::Blocked);
        assert!(result.status.permits_continuation());
        assert!(result.summary.contains("outside project root"));
    }

    #[test]
    fn tree_path_error_maps_inaccessible_root_to_recoverable() {
        let result = tree_path_error(
            "ls-tree",
            "src",
            PathError::RootInaccessible {
                root: Utf8PathBuf::from("/missing"),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
            },
        );
        assert_eq!(result.status, CommandStatus::RecoverableFailure);
        assert!(result.status.permits_continuation());
    }

    /// Fresh temp root; returns the canonical root and the guard that keeps it
    /// alive. `tempfile` honours `TMPDIR` (`/var/tmp` per `.cargo/config.toml`),
    /// which is not a git repository — so the `.gitignore` tests also prove
    /// `require_git(false)` is doing its job.
    fn temp_root() -> (Utf8PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 temp path");
        let root = root.canonicalize_utf8().expect("canonicalize temp root");
        (root, dir)
    }

    fn write_file(root: &Utf8Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    async fn run_tree(root: &Utf8Path, arguments: &[&str]) -> CommandResult {
        let context = ShellContext::new(root.to_owned());
        let invocation = CommandInvocation {
            command: "ls-tree".to_owned(),
            arguments: arguments.iter().map(|argument| (*argument).to_owned()).collect(),
            raw: format!("ls-tree {}", arguments.join(" ")),
        };
        let services = CommandServices::new(CommandRegistry::new(), ShellHistory::new(16));
        TreeCommand::new().execute(&invocation, &context, &services, CancellationToken::new()).await
    }

    fn entry_paths(result: &CommandResult) -> Vec<String> {
        result.data["entries"]
            .as_array()
            .expect("entries array")
            .iter()
            .map(|entry| entry["path"].as_str().expect("entry path").to_owned())
            .collect()
    }

    // Only the symlink test below consumes this, and that test is unix-only
    // (creating symlinks on Windows needs elevated privileges). Without a
    // matching cfg this helper is dead code on Windows, which
    // `#![deny(dead_code)]` in lib.rs rejects.
    #[cfg(unix)]
    fn entry_kinds(result: &CommandResult) -> Vec<(String, String)> {
        result.data["entries"]
            .as_array()
            .expect("entries array")
            .iter()
            .map(|entry| {
                (
                    entry["path"].as_str().expect("entry path").to_owned(),
                    entry["kind"].as_str().expect("entry kind").to_owned(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn root_gitignore_hides_target_and_dotfiles_remain_listed() {
        let (root, _dir) = temp_root();
        assert!(!root.join(".git").exists(), "fixture must not be a git repo");
        write_file(&root, ".gitignore", "target/\n");
        write_file(&root, "src/main.rs", "fn main() {}");
        write_file(&root, "target/debug/app.rlib", "artifact");
        write_file(&root, "target/debug/app.rmeta", "artifact");

        let result = run_tree(&root, &[]).await;
        assert_eq!(result.status, CommandStatus::Succeeded);
        let paths = entry_paths(&result);
        assert!(paths.contains(&"src/main.rs".to_owned()));
        assert!(!paths.iter().any(|path| path.starts_with("target")));
        // `.hidden(false)`: the `.gitignore` dotfile itself is still listed.
        assert!(paths.contains(&".gitignore".to_owned()));
    }

    #[tokio::test]
    async fn nested_gitignore_applies_only_to_its_subtree() {
        let (root, _dir) = temp_root();
        write_file(&root, "app/.gitignore", "generated/\n");
        write_file(&root, "app/generated/code.rs", "// generated");
        write_file(&root, "app/src/lib.rs", "pub fn x() {}");
        write_file(&root, "other/generated/keep.rs", "// kept");

        let result = run_tree(&root, &[]).await;
        let paths = entry_paths(&result);
        assert!(!paths.iter().any(|path| path.starts_with("app/generated")));
        assert!(paths.contains(&"app/src/lib.rs".to_owned()));
        // A sibling subtree without its own ignore rule is unaffected.
        assert!(paths.contains(&"other/generated/keep.rs".to_owned()));
    }

    #[tokio::test]
    async fn hidden_files_are_still_listed() {
        let (root, _dir) = temp_root();
        write_file(&root, ".hidden", "x");
        write_file(&root, "visible.txt", "y");

        let result = run_tree(&root, &[]).await;
        let paths = entry_paths(&result);
        assert!(paths.contains(&".hidden".to_owned()));
        assert!(paths.contains(&"visible.txt".to_owned()));
    }

    #[tokio::test]
    async fn ripgrep_ignore_file_does_not_hide_entries() {
        let (root, _dir) = temp_root();
        write_file(&root, ".ignore", "dot-ignore-target.txt\n");
        write_file(&root, "dot-ignore-target.txt", "still listed");

        let result = run_tree(&root, &[]).await;
        let paths = entry_paths(&result);
        // `.ignore(false)`: a ripgrep-style `.ignore` must not alter the result.
        assert!(paths.contains(&"dot-ignore-target.txt".to_owned()));
        // The `.ignore` file itself is an ordinary dotfile and stays listed.
        assert!(paths.contains(&".ignore".to_owned()));
    }

    #[test]
    fn ignored_directory_is_pruned_and_does_not_consume_budget() {
        let (root, _dir) = temp_root();
        write_file(&root, ".gitignore", "ignored/\n");
        for index in 0..100 {
            write_file(&root, &format!("ignored/artifact-{index:03}.rs"), "// ignored");
        }
        write_file(&root, "keep-a.rs", "a");
        write_file(&root, "keep-b.rs", "b");

        // Cap large enough for ".", ".gitignore" and the two kept files, but far
        // too small for 100 ignored artifacts. Pruning (rather than descending
        // and post-filtering) is what keeps the ignored files off the budget.
        let outcome = collect_tree_entries(&root, &root, 8, 8, &CancellationToken::new());
        let TreeWalk::Collected { entries, diagnostics } = outcome else {
            panic!("walk must complete without cancellation");
        };
        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert!(paths.contains(&"keep-a.rs"));
        assert!(paths.contains(&"keep-b.rs"));
        assert!(!paths.iter().any(|path| path.starts_with("ignored")));
        assert!(!diagnostics.iter().any(|diagnostic| diagnostic.code == "shell.tree.truncated"));
    }

    #[test]
    fn truncation_warning_fires_for_non_ignored_entries() {
        let (root, _dir) = temp_root();
        for index in 0..20 {
            write_file(&root, &format!("file-{index:02}.rs"), "x");
        }

        let outcome = collect_tree_entries(&root, &root, 4, 5, &CancellationToken::new());
        let TreeWalk::Collected { entries, diagnostics } = outcome else {
            panic!("walk must complete without cancellation");
        };
        assert_eq!(entries.len(), 5);
        assert!(diagnostics.iter().any(|diagnostic| diagnostic.code == "shell.tree.truncated"));
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let (root, _dir) = temp_root();
        for index in 0..10 {
            write_file(&root, &format!("file-{index}.rs"), "x");
        }
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = collect_tree_entries(&root, &root, 4, 100, &cancel);
        assert!(matches!(outcome, TreeWalk::Cancelled));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_blocked_and_links_are_not_followed() {
        let (root, _dir) = temp_root();
        let outside = tempfile::tempdir().expect("outside dir");
        let outside = Utf8PathBuf::from_path_buf(outside.path().to_path_buf())
            .expect("utf8 outside path")
            .canonicalize_utf8()
            .expect("canonicalize outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("symlink");
        write_file(&root, "inside.txt", "inside");

        // The root-containment gate runs on the requested target first.
        let blocked = run_tree(&root, &["escape"]).await;
        assert_eq!(blocked.status, CommandStatus::Blocked);

        // The walk lists the link as a symlink but never follows it out of root.
        let listed = run_tree(&root, &[]).await;
        let paths = entry_paths(&listed);
        assert!(paths.contains(&"escape".to_owned()));
        assert!(!paths.iter().any(|path| path.starts_with("escape/")));
        assert!(!paths.iter().any(|path| path.contains("secret.txt")));
        assert!(paths.contains(&"inside.txt".to_owned()));
        let kinds = entry_kinds(&listed);
        assert!(kinds.contains(&("escape".to_owned(), "symlink".to_owned())));
    }
}
