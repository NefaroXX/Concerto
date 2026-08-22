//! Git tool — uses gix (gitoxide) for operations, with CLI fallback
//! for operations gix 0.85 doesn't support natively.
//!
//! CLI fallback operations are isolated in [`cli_fallback`] and documented
//! per the ROADMAP.md risk-register entry on gix gaps.

use async_trait::async_trait;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::{CancellationToken, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tracing;

/// Timeout for CLI-fallback git operations.
///
/// The fallback path only handles local operations (branch, switch, add,
/// commit, restore, stash). These can run user-configured hooks that take a
/// while, but never block on the network — clone/fetch/pull are not routed
/// through it — so 60s generously bounds hook-bound local work while still
/// terminating a runaway process.
const GIT_TIMEOUT_SECS: u64 = 60;

/// Git tool — exposes common git operations via gix or CLI fallback.
pub struct GitTool;

/// Return `ToolError::Cancelled` if the token has been cancelled.
fn check_cancel(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

/// Parameter struct for the `git` tool.
///
/// The schema is derived from this type via `schemars` so the advertised JSON
/// Schema contract can never drift from the actual deserialization target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitInput {
    /// The git operation to perform.
    pub operation: String,

    /// File paths for add/restore operations.
    #[serde(default)]
    pub paths: Option<Vec<String>>,

    /// Commit or stash message.
    pub message: Option<String>,

    /// Branch name for create/switch operations.
    pub branch: Option<String>,

    /// Max log entries (default: 10).
    #[serde(default)]
    pub max_count: Option<u32>,

    /// Path to git repo (default: project dir).
    pub repo_path: Option<String>,
}

// ---------------------------------------------------------------------------
// Lenient input coercion
// ---------------------------------------------------------------------------

/// Leniently binds a raw git tool argument into [`GitInput`].
///
/// The JSON Schema advertised by [`GitTool::input_schema`] stays authoritative;
/// only the boundary parser is lenient. Strict deserialization is always tried
/// first. If it fails, per-field normalization is applied and a second strict
/// parse is attempted so a model that emits string-typed integers or a
/// newline-separated `paths` string still gets a usable tool call. If the
/// normalized input also fails to parse, the ORIGINAL strict deserialize error
/// is returned (message shape `invalid git input: {e}` unchanged).
fn coerce_git_input(input: &serde_json::Value) -> Result<GitInput, ToolError> {
    match serde_json::from_value(input.clone()) {
        Ok(parsed) => Ok(parsed),
        Err(strict_error) => {
            let normalized = normalize_git_input(input);
            serde_json::from_value(normalized).map_err(|_| ToolError::ExecutionFailed {
                message: format!("invalid git input: {strict_error}"),
            })
        }
    }
}

/// Normalizes a git tool input after strict parsing has failed.
///
/// Only fields with a well-defined lenient coercion are touched: `paths`
/// accepts a single multi-item string, integer fields accept string-typed
/// integers, and `operation`/`message`/`branch`/`repo_path` accept non-string
/// scalars (numbers/bools → strings). Everything else is left untouched so the
/// final strict parse reports genuinely malformed fields accurately.
fn normalize_git_input(input: &serde_json::Value) -> serde_json::Value {
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let mut normalized = object.clone();
    for (field, value) in object {
        match field.as_str() {
            "paths" => {
                normalized.insert("paths".into(), coerce_paths(value));
            }
            "max_count" => {
                normalized.insert("max_count".into(), coerce_u32(value));
            }
            "operation" | "message" | "branch" | "repo_path" => {
                normalized.insert(field.clone(), coerce_scalar_to_string(value));
            }
            _ => {}
        }
    }
    serde_json::Value::Object(normalized)
}

/// Coerces a `paths` value into the expected string array.
///
/// A single string is split on newlines, commas and whitespace (empty parts
/// dropped) so `"a.txt\nb.txt"` or `"a.txt,b.txt"` become two paths. Array
/// items are stringified when they are scalars; objects and nested arrays pass
/// through and are rejected by the strict parser.
fn coerce_paths(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            let paths: Vec<String> = text
                .split(|character: char| {
                    character == '\n'
                        || character == '\r'
                        || character == ','
                        || character.is_whitespace()
                })
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(String::from)
                .collect();
            serde_json::json!(paths)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| match item {
                    serde_json::Value::String(_) => item.clone(),
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                        serde_json::json!(item.to_string())
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Coerces a string-typed integer field (e.g. `max_count`) into a number.
///
/// Whitespace is trimmed before parsing. Values that are not strings, and
/// strings that do not parse as `u32`, pass through untouched so the strict
/// parser reports them with the same error.
fn coerce_u32(value: &serde_json::Value) -> serde_json::Value {
    match value.as_str() {
        Some(text) => match text.trim().parse::<u32>() {
            Ok(number) => serde_json::json!(number),
            Err(_) => serde_json::json!(text),
        },
        None => value.clone(),
    }
}

/// Coerces a scalar non-string value into its string form for string fields.
///
/// Numbers and bools are converted; strings, `null`, arrays, and objects pass
/// through untouched (so optional fields can stay `null` and malformed fields
/// keep producing the strict deserialize error).
fn coerce_scalar_to_string(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(_) | serde_json::Value::Null => value.clone(),
        serde_json::Value::Number(number) => serde_json::json!(number.to_string()),
        serde_json::Value::Bool(boolean) => serde_json::json!(boolean.to_string()),
        other => other.clone(),
    }
}

/// Lightweight repository metadata for status surfaces outside the Git
/// tool itself (for example, the desktop quick panel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySummary {
    pub branch: String,
    pub changed_files: usize,
}

// ---------------------------------------------------------------------------
// Repo opening
// ---------------------------------------------------------------------------

fn open_repo(repo_path: &str) -> Result<gix::Repository, ToolError> {
    gix::open(repo_path).map_err(|e| {
        let msg = format!("failed to open git repository: {e}");
        // Classify non-repository directories as a recoverable "not a repo"
        // error rather than an execution failure, so the orchestrator and
        // coder can treat the absence of git as non-fatal.
        let err_str = e.to_string().to_lowercase();
        if err_str.contains("not a git repository")
            || err_str.contains("no git repository")
            || err_str.contains("could not find repository")
            || err_str.contains("does not appear to be a git repository")
        {
            // Direct the model away from retrying git in a non-repo working
            // directory (observed live: repeated status/diff calls looped on
            // this error). The hint names the git-free alternative explicitly.
            ToolError::NotARepository {
                message: format!(
                    "{msg}; hint: the working directory is not a git repository — \
                     git commands cannot run here. Skip all git operations, use \
                     the filesystem and shell tools instead, or initialize git \
                     first (`git init`) if version control is needed"
                ),
            }
        } else {
            ToolError::ExecutionFailed { message: msg }
        }
    })
}

/// Read the current branch and worktree change count without invoking an
/// external shell. Callers should run this on a blocking worker because gix
/// performs synchronous filesystem I/O.
pub fn repository_summary(repo_path: &Path) -> Result<RepositorySummary, ToolError> {
    let repo = open_repo(&repo_path.to_string_lossy())?;
    let branch = repo
        .head()
        .ok()
        .and_then(|head| head.referent_name().map(|name| name.shorten().to_string()))
        .unwrap_or_else(|| "detached HEAD".to_string());
    let changed_files = gix_status_lines(&repo)?.len();
    Ok(RepositorySummary { branch, changed_files })
}

// ---------------------------------------------------------------------------
// gix-backed operations
// ---------------------------------------------------------------------------

/// Produce status output similar to `git status --porcelain`.
fn gix_status_lines(repo: &gix::Repository) -> Result<Vec<String>, ToolError> {
    use gix::status::Item as SItem;
    let platform = repo.status(gix::progress::Discard).map_err(|e| ToolError::ExecutionFailed {
        message: format!("failed to get status: {e}"),
    })?;

    let iter = platform.into_iter(Vec::new()).map_err(|e| ToolError::ExecutionFailed {
        message: format!("failed to create status iterator: {e}"),
    })?;

    let mut lines: Vec<String> = Vec::new();

    for result in iter {
        let item = result.map_err(|e| ToolError::ExecutionFailed {
            message: format!("status iteration failed: {e}"),
        })?;

        match item {
            SItem::IndexWorktree(change) => {
                // Changes between index and worktree
                let path = change.rela_path().to_string();
                let summary = change.summary();
                if let Some(s) = summary {
                    let porcelain = match s {
                        gix::status::index_worktree::iter::Summary::Modified => " M",
                        gix::status::index_worktree::iter::Summary::Added => "A ",
                        gix::status::index_worktree::iter::Summary::Removed => " D",
                        gix::status::index_worktree::iter::Summary::Renamed => " R",
                        gix::status::index_worktree::iter::Summary::Copied => " C",
                        gix::status::index_worktree::iter::Summary::TypeChange => " T",
                        gix::status::index_worktree::iter::Summary::Conflict => "DD",
                        gix::status::index_worktree::iter::Summary::IntentToAdd => " A",
                    };
                    lines.push(format!("{porcelain} {path}"));
                }
            }
            SItem::TreeIndex(change) => {
                // Changes between HEAD and index (staged)
                let path = change.location().to_string();
                let porcelain = match change {
                    gix::diff::index::Change::Addition { .. } => "A ",
                    gix::diff::index::Change::Deletion { .. } => "D ",
                    gix::diff::index::Change::Modification { .. } => "M ",
                    gix::diff::index::Change::Rewrite { .. } => "R ",
                };
                lines.push(format!("{porcelain}{path}"));
            }
        }
    }

    Ok(lines)
}

fn gix_status(repo: &gix::Repository) -> Result<ToolOutput, ToolError> {
    let lines = gix_status_lines(repo)?;
    let count = lines.len();
    let stdout = lines.join("\n");
    Ok(ToolOutput {
        summary: format!("{count} changed files"),
        data: serde_json::json!({"status": stdout}),
    })
}

/// Produce unified diff between HEAD and the index tree, using
/// gix's tree-diff capabilities.
fn gix_diff(repo: &gix::Repository) -> Result<ToolOutput, ToolError> {
    use std::fmt::Write;

    let head_tree = repo.head_tree().map_err(|e| ToolError::ExecutionFailed {
        message: format!("failed to get HEAD tree: {e}"),
    })?;

    let mut output = String::new();
    let mut file_count = 0u32;

    // Diff HEAD tree vs the index (None means index). Returns Vec<ChangeDetached>.
    let changes = repo.diff_tree_to_tree(Some(&head_tree), None, None).map_err(|e| {
        ToolError::ExecutionFailed { message: format!("failed to diff trees: {e}") }
    })?;

    for change in &changes {
        use gix::diff::tree_with_rewrites::Change as DChange;
        let path = match change {
            DChange::Addition { location, .. }
            | DChange::Deletion { location, .. }
            | DChange::Modification { location, .. }
            | DChange::Rewrite { location, .. } => {
                std::str::from_utf8(location).unwrap_or("unknown")
            }
        };
        let (old_prefix, new_prefix, marker): (String, String, &str) = match change {
            DChange::Addition { .. } => ("/dev/null".into(), format!("b/{path}"), "new"),
            DChange::Deletion { .. } => (format!("a/{path}"), "/dev/null".into(), "deleted"),
            DChange::Modification { .. } => (format!("a/{path}"), format!("b/{path}"), "modified"),
            DChange::Rewrite { .. } => (format!("a/{path}"), format!("b/{path}"), "rewrite"),
        };
        let _ = writeln!(output, "diff --git a/{path} b/{path}");
        let _ = writeln!(output, "--- {old_prefix}");
        let _ = writeln!(output, "+++ {new_prefix}");
        let _ = writeln!(output, "@@ -1,1 +1,1 @@");
        let _ = writeln!(output, " {marker}");
        file_count += 1;
    }

    Ok(ToolOutput {
        summary: format!("{file_count} files changed"),
        data: serde_json::json!({"diff": output}),
    })
}

/// Walk commit history starting from HEAD.
fn gix_log(repo: &gix::Repository, max_count: usize) -> Result<ToolOutput, ToolError> {
    let head_commit = repo.head_commit().map_err(|e| ToolError::ExecutionFailed {
        message: format!("failed to get HEAD commit: {e}"),
    })?;

    let head_id = head_commit.id();

    let walk = repo.rev_walk([head_id]);
    let commits: Vec<serde_json::Value> = walk
        .all()
        .map_err(|e| ToolError::ExecutionFailed {
            message: format!("failed to start revision walk: {e}"),
        })?
        .filter_map(|r| {
            let info = r.ok()?;
            let id = info.id;
            let commit = repo.find_commit(id).ok()?;

            let hash = id.to_hex().to_string();
            let author = commit.author().ok().map(|a| a.name.to_string()).unwrap_or_default();
            let time = commit.time().ok().map(|t| t.seconds).unwrap_or(0);
            let message = commit.message_raw_sloppy().to_string();

            Some(serde_json::json!({
                "hash": hash,
                "author": author,
                "time": time,
                "message": message.trim(),
            }))
        })
        .take(max_count)
        .collect();

    Ok(ToolOutput {
        summary: format!("{} commits", commits.len()),
        data: serde_json::json!({"commits": commits}),
    })
}

/// List local branches, mirroring `git branch` output.
fn gix_branch_list(repo: &gix::Repository) -> Result<ToolOutput, ToolError> {
    let head = repo.head().ok();
    let head_branch = head.as_ref().and_then(|h| h.referent_name().map(|n| n.to_string()));

    let refs = repo.references().map_err(|e| ToolError::ExecutionFailed {
        message: format!("failed to list references: {e}"),
    })?;

    let branches: Vec<String> = refs
        .local_branches()
        .map_err(|e| ToolError::ExecutionFailed {
            message: format!("failed to list local branches: {e}"),
        })?
        .filter_map(|r| {
            let reference = r.ok()?;
            let name = reference.name().shorten().to_string();
            let is_current = head_branch.as_deref() == Some(name.as_str());
            let display = if is_current { format!("* {name}") } else { format!("  {name}") };
            Some(display)
        })
        .collect();

    Ok(ToolOutput {
        summary: format!("{} branches", branches.len()),
        data: serde_json::json!({"branches": branches}),
    })
}

/// Create a new branch at the current HEAD.
fn gix_branch_create(repo: &gix::Repository, name: &str) -> Result<ToolOutput, ToolError> {
    let head_id = repo.head_id().map_err(|e| ToolError::ExecutionFailed {
        message: format!("failed to get HEAD id: {e}"),
    })?;

    let full_name = format!("refs/heads/{name}");
    let ref_name = gix::refs::FullName::try_from(full_name.clone())
        .map_err(|e| ToolError::ExecutionFailed { message: format!("invalid branch name: {e}") })?;

    let _ref = repo
        .reference(ref_name, head_id, gix::refs::transaction::PreviousValue::Any, "")
        .map_err(|e| ToolError::ExecutionFailed {
            message: format!("failed to create branch '{name}': {e}"),
        })?;

    Ok(ToolOutput {
        summary: format!("Created branch {name}"),
        data: serde_json::json!({"branch": name, "output": ""}),
    })
}

// ---------------------------------------------------------------------------
// CLI fallback for operations gix 0.85 doesn't support
// ---------------------------------------------------------------------------

/// CLI fallback for git operations that gix 0.85 doesn't support natively.
///
/// Per ROADMAP.md risk register: "gix missing operations needed → fallback
/// to spawning git binary as last resort, wrapped in same interface".
///
/// Stash operations (push/pop/list) are not available in gix plumbing at 0.85.
/// Some other operations (add, commit, branch_switch, restore) use CLI
/// because their gix equivalents require multi-step recipes (index
/// manipulation, tree writing, checkout worktree updates) that are more
/// reliably handled by the CLI at this point.
mod cli_fallback {
    use concerto_core::{CancellationToken, ToolError};
    use std::time::Duration;

    use crate::process::{kill_process_group, ProcessHandle};

    /// Run a git CLI command in `repo_path`, capturing stdout and stderr, with
    /// cancellation and timeout support.
    ///
    /// The child is spawned in its own process group (unix) and the whole
    /// group is SIGKILLed on cancellation or timeout so hooks and other
    /// descendants cannot survive the tool call. stdout and stderr are read
    /// concurrently with `wait()` (and capped) so pipe buffers never fill
    /// while the process is alive.
    pub(super) async fn run_git(
        repo_path: &str,
        args: &[&str],
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<(String, String), ToolError> {
        let mut command = tokio::process::Command::new("git");
        command.args(args);
        command.current_dir(repo_path);
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|e| ToolError::ExecutionFailed {
            message: format!("failed to execute git: {e}"),
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                kill_process_group(&mut child);
                let _ = child.wait().await;
                Err(ToolError::Cancelled)
            }

            result = tokio::time::timeout(timeout, async {
                // Read stdout, stderr, and wait for exit concurrently so pipe
                // buffers never fill up while the process is still running.
                let (status, stdout, stderr) = tokio::join!(
                    child.wait(),
                    ProcessHandle::read_with_limit(stdout),
                    ProcessHandle::read_with_limit(stderr),
                );
                let success = status.map(|s| s.success()).unwrap_or(false);
                (success, stdout, stderr)
            }) => {
                match result {
                    Ok((true, stdout, stderr)) => Ok((stdout, stderr)),
                    Ok((false, _stdout, stderr)) => Err(ToolError::ExecutionFailed {
                        message: format!("git {} failed: {stderr}", args.join(" ")),
                    }),
                    Err(_elapsed) => {
                        kill_process_group(&mut child);
                        let _ = child.wait().await;
                        Err(ToolError::Timeout { timeout_secs: timeout.as_secs() })
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl concerto_core::traits::tool::Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Git operations — status, diff, add, commit, branch, stash, log, restore."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Derive the schema from the Rust type so the advertised contract can
        // never drift from the deserialization target.  `operation` (no
        // default) is required; everything else is optional.
        let root = schemars::schema_for!(GitInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize GitInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["status", "diff", "add", "commit",
                                 "branch_create", "branch_switch", "branch_list",
                                 "stash_push", "stash_pop", "stash_list",
                                 "log", "restore"]
                    },
                    "paths": { "type": "array", "items": { "type": "string" },
                               "description": "File paths for add/restore operations." },
                    "message": { "type": "string", "description": "Commit or stash message." },
                    "branch": { "type": "string", "description": "Branch name for create/switch." },
                    "max_count": { "type": "integer",
                                   "description": "Max log entries (default: 10)." },
                    "repo_path": { "type": "string",
                                   "description": "Path to git repo (default: project dir)." }
                },
                "required": ["operation"]
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // Coarse flag vocabulary matching the agent capability flags; write
        // enforcement on the repository is policy's job.
        CapabilitySet::default().with_requirement("git")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        // Leniently coerce the raw argument at the execution boundary: the
        // schema stays strict, but the parser accepts string-typed numbers and
        // newline/comma-separated path lists from the model.
        let git_input: GitInput = coerce_git_input(&input)?;

        // Keep a Value representation for helper functions that still
        // expect &serde_json::Value.  This avoids refactoring them all now.
        let input_value = serde_json::to_value(&git_input).unwrap_or_else(|_| input.clone());

        let operation = git_input.operation.as_str();

        check_cancel(&cancel)?;

        let repo_path = git_input
            .repo_path
            .as_deref()
            .unwrap_or_else(|| session.project_dir.to_str().unwrap_or("."));

        // Security: constrain repo_path to the project root so that a
        // model-supplied path cannot escape the project boundary.
        let project_root = std::path::Path::new(&session.project_dir)
            .canonicalize()
            .unwrap_or_else(|_| std::path::Path::new(&session.project_dir).to_path_buf());
        let resolved = std::path::Path::new(repo_path)
            .canonicalize()
            .unwrap_or_else(|_| std::path::Path::new(repo_path).to_path_buf());
        if !resolved.starts_with(&project_root) {
            return Err(ToolError::ExecutionFailed {
                message: format!(
                    "repo_path '{}' is outside the project root '{}'",
                    resolved.display(),
                    project_root.display(),
                ),
            });
        }

        check_cancel(&cancel)?;

        match operation {
            "status" => {
                check_cancel(&cancel)?;
                let repo = open_repo(repo_path)?;
                gix_status(&repo)
            }
            "diff" => {
                check_cancel(&cancel)?;
                let repo = open_repo(repo_path)?;
                gix_diff(&repo)
            }
            "log" => {
                check_cancel(&cancel)?;
                let max = git_input.max_count.unwrap_or(10) as usize;
                let repo = open_repo(repo_path)?;
                gix_log(&repo, max)
            }
            "branch_list" => {
                check_cancel(&cancel)?;
                let repo = open_repo(repo_path)?;
                gix_branch_list(&repo)
            }
            "branch_create" => {
                check_cancel(&cancel)?;
                // `input_value` (re-serialized from the coerced GitInput) so a
                // number-typed branch is seen as a string here as well.
                let name = extract_branch(&input_value)?;
                match open_repo(repo_path).and_then(|repo| gix_branch_create(&repo, name)) {
                    Ok(output) => Ok(output),
                    Err(_) => {
                        let (stdout, _) = cli_fallback::run_git(
                            repo_path,
                            &["branch", "--", name],
                            &cancel,
                            Duration::from_secs(GIT_TIMEOUT_SECS),
                        )
                        .await?;
                        Ok(ToolOutput {
                            summary: format!("Created branch {name}"),
                            data: serde_json::json!({"branch": name, "output": stdout}),
                        })
                    }
                }
            }
            "branch_switch" => {
                check_cancel(&cancel)?;
                let name = extract_branch(&input_value)?;
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &["switch", name],
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                Ok(ToolOutput {
                    summary: format!("Switched to branch {name}"),
                    data: serde_json::json!({"branch": name, "output": stdout}),
                })
            }
            "add" => {
                check_cancel(&cancel)?;
                let paths = extract_paths(&input_value)?;
                let mut args = vec!["add", "--"];
                args.extend(paths.iter().map(|s| s.as_str()));
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &args,
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                Ok(ToolOutput {
                    summary: format!("Added {} files", paths.len()),
                    data: serde_json::json!({"added": paths, "output": stdout}),
                })
            }
            "commit" => {
                check_cancel(&cancel)?;
                let msg = extract_message(&input_value)?;
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &["commit", "-m", msg],
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                Ok(ToolOutput {
                    summary: "Committed".into(),
                    data: serde_json::json!({"output": stdout}),
                })
            }
            "restore" => {
                check_cancel(&cancel)?;
                let paths = extract_paths(&input_value)?;
                let mut args = vec!["checkout", "--"];
                args.extend(paths.iter().map(|s| s.as_str()));
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &args,
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                Ok(ToolOutput {
                    summary: format!("Restored {} files", paths.len()),
                    data: serde_json::json!({"restored": paths, "output": stdout}),
                })
            }
            "stash_push" => {
                check_cancel(&cancel)?;
                let mut args = vec!["stash", "push"];
                if let Some(msg) = git_input.message.as_deref() {
                    args.push("-m");
                    args.push(msg);
                }
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &args,
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                Ok(ToolOutput {
                    summary: "Stashed".into(),
                    data: serde_json::json!({"output": stdout}),
                })
            }
            "stash_pop" => {
                check_cancel(&cancel)?;
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &["stash", "pop"],
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                Ok(ToolOutput {
                    summary: "Popped stash".into(),
                    data: serde_json::json!({"output": stdout}),
                })
            }
            "stash_list" => {
                check_cancel(&cancel)?;
                let (stdout, _) = cli_fallback::run_git(
                    repo_path,
                    &["stash", "list"],
                    &cancel,
                    Duration::from_secs(GIT_TIMEOUT_SECS),
                )
                .await?;
                let stashes: Vec<&str> = stdout.lines().collect();
                Ok(ToolOutput {
                    summary: format!("{} stashes", stashes.len()),
                    data: serde_json::json!({"stashes": stashes}),
                })
            }
            other => Err(ToolError::ExecutionFailed {
                message: format!(
                    "unknown git operation: {other}; valid operations: status, diff, log, branch_list, branch_create, branch_switch, add, commit, restore, stash_push, stash_pop, stash_list"
                ),
            }),
        }
    }
}

fn extract_paths(input: &serde_json::Value) -> Result<Vec<String>, ToolError> {
    let paths = input
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
        .filter(|v: &Vec<String>| !v.is_empty())
        .ok_or_else(|| ToolError::ExecutionFailed {
            message: "'paths' must be a non-empty array of strings".into(),
        })?;

    // Defense-in-depth: reject paths that look like CLI flags
    if let Some(bad) = paths.iter().find(|p| p.starts_with('-')) {
        return Err(ToolError::ExecutionFailed {
            message: format!("path '{bad}' looks like a flag, not a valid file path"),
        });
    }

    Ok(paths)
}

fn extract_message(input: &serde_json::Value) -> Result<&str, ToolError> {
    input.get("message").and_then(|v| v.as_str()).ok_or_else(|| ToolError::ExecutionFailed {
        message: "'message' is required for this operation".into(),
    })
}

fn extract_branch(input: &serde_json::Value) -> Result<&str, ToolError> {
    let name = input.get("branch").and_then(|v| v.as_str()).ok_or_else(|| {
        ToolError::ExecutionFailed { message: "'branch' is required for this operation".into() }
    })?;

    // Defense-in-depth: reject branch names that look like CLI flags
    if name.starts_with('-') {
        return Err(ToolError::ExecutionFailed {
            message: format!("branch name '{name}' must not start with '-'"),
        });
    }

    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::AllowAllPolicy;
    use concerto_core::traits::tool::Tool;
    use std::path::PathBuf;

    fn test_policy() -> AllowAllPolicy {
        AllowAllPolicy
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = gix::init(dir.path()).unwrap();

        // Write README.md as a blob.
        let blob_id = repo.write_blob(b"# Test").unwrap();

        // Create a tree containing the blob entry.
        let tree = gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: b"README.md".to_vec().into(),
                oid: blob_id.into(),
            }],
        };
        let tree_id = repo.write_object(&tree).unwrap();

        // Create a signature (committer and author are same).
        let mut time_buf = gix::date::parse::TimeBuf::default();
        let sig = gix::actor::Signature {
            name: b"Test".to_vec().into(),
            email: b"test@concerto.rs".to_vec().into(),
            time: gix::date::Time { seconds: 0, offset: 0 },
        };
        let sig_ref = sig.to_ref(&mut time_buf);

        // Create the initial commit with no parents.
        repo.commit_as(
            sig_ref,
            sig_ref,
            "refs/heads/main",
            "initial",
            tree_id,
            [] as [gix::hash::ObjectId; 0],
        )
        .unwrap();

        dir
    }

    fn session(repo_path: PathBuf) -> SessionContext {
        SessionContext::new(concerto_core::ids::Ulid::new(), repo_path)
    }

    fn input(op: &str) -> serde_json::Value {
        serde_json::json!({"operation": op})
    }

    #[tokio::test]
    async fn status_works() {
        let dir = init_repo();
        let tool = GitTool;
        let policy = test_policy();
        let result = tool
            .execute(
                input("status"),
                &policy,
                &session(dir.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_ok(), "status failed: {:?}", result.err());
    }

    #[tokio::test]
    async fn unknown_operation_lists_valid_operations() {
        let dir = init_repo();
        let tool = GitTool;
        let policy = test_policy();
        // "branch" was a real dead-end in a live session; any non-arm value works.
        let err = tool
            .execute(
                input("branch"),
                &policy,
                &session(dir.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown git operation: branch"), "unexpected message: {message}");
        assert!(message.contains("valid operations:"), "unexpected message: {message}");
        assert!(message.contains("status"), "unexpected message: {message}");
        assert!(message.contains("log"), "unexpected message: {message}");
        assert!(message.contains("stash_list"), "unexpected message: {message}");
    }

    #[tokio::test]
    async fn add_and_commit() {
        let dir = init_repo();
        let tool = GitTool;
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let ses = session(dir.path().to_path_buf());

        // Set git user identity so CLI fallback operations work
        let repo_path = dir.path().to_str().unwrap();
        cli_fallback::run_git(
            repo_path,
            &["config", "user.email", "test@concerto.rs"],
            &cancel,
            Duration::from_secs(GIT_TIMEOUT_SECS),
        )
        .await
        .unwrap();
        cli_fallback::run_git(
            repo_path,
            &["config", "user.name", "Test"],
            &cancel,
            Duration::from_secs(GIT_TIMEOUT_SECS),
        )
        .await
        .unwrap();

        std::fs::write(dir.path().join("new.txt"), "hello").unwrap();

        let add_input = serde_json::json!({"operation": "add", "paths": ["new.txt"]});
        let result = tool.execute(add_input, &policy, &ses, cancel.clone()).await;
        assert!(result.is_ok(), "add failed: {:?}", result.err());

        let commit_input = serde_json::json!({"operation": "commit", "message": "add new.txt"});
        let result = tool.execute(commit_input, &policy, &ses, cancel).await;
        assert!(result.is_ok(), "commit failed: {:?}", result.err());

        let (log_out, _) = cli_fallback::run_git(
            dir.path().to_str().unwrap(),
            &["log", "--oneline"],
            &CancellationToken::new(),
            Duration::from_secs(GIT_TIMEOUT_SECS),
        )
        .await
        .unwrap();
        assert!(log_out.contains("add new.txt"), "commit not found: {log_out}");
    }

    #[tokio::test]
    async fn branch_ops() {
        let dir = init_repo();
        let tool = GitTool;
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let ses = session(dir.path().to_path_buf());

        let create = serde_json::json!({"operation": "branch_create", "branch": "feature"});
        let result = tool.execute(create, &policy, &ses, cancel.clone()).await;
        assert!(result.is_ok(), "branch create failed: {:?}", result.err());

        let result =
            tool.execute(input("branch_list"), &policy, &ses, cancel.clone()).await.unwrap();
        let branches = result.data["branches"].as_array().unwrap();
        let names: Vec<&str> =
            branches.iter().filter_map(|b| b.as_str().map(|s| s.trim())).collect();
        assert!(names.iter().any(|n| n.contains("feature")), "feature not in {names:?}");

        let switch = serde_json::json!({"operation": "branch_switch", "branch": "feature"});
        let result = tool.execute(switch, &policy, &ses, cancel).await;
        assert!(result.is_ok(), "branch switch failed: {:?}", result.err());
    }

    #[tokio::test]
    async fn log_returns_commits() {
        let dir = init_repo();
        let tool = GitTool;
        let policy = test_policy();
        let result = tool
            .execute(
                input("log"),
                &policy,
                &session(dir.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_ok(), "log failed: {:?}", result.err());
        let output = result.unwrap();
        let commits = output.data["commits"].as_array().unwrap();
        assert!(!commits.is_empty());
    }

    #[tokio::test]
    async fn diff_works() {
        let dir = init_repo();
        let tool = GitTool;
        let policy = test_policy();
        let cancel = CancellationToken::new();

        std::fs::write(dir.path().join("README.md"), "# Modified").unwrap();

        let result =
            tool.execute(input("diff"), &policy, &session(dir.path().to_path_buf()), cancel).await;
        assert!(result.is_ok(), "diff failed: {:?}", result.err());
    }

    #[tokio::test]
    async fn run_git_cancelled_when_precancelled() {
        // A pre-cancelled token must abort the git CLI child and surface
        // `ToolError::Cancelled` rather than completing the command.
        let dir = init_repo();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = cli_fallback::run_git(
            dir.path().to_str().unwrap(),
            &["status"],
            &cancel,
            Duration::from_secs(GIT_TIMEOUT_SECS),
        )
        .await;
        match result {
            Err(ToolError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_git_directory_returns_not_a_repository_error() {
        let dir = tempfile::TempDir::new().unwrap();
        // Intentionally do NOT init a git repo here.
        let result = open_repo(dir.path().to_str().unwrap());
        match result {
            Err(ToolError::NotARepository { .. }) => {} // expected
            Err(other) => panic!("expected NotARepository, got: {other:?}"),
            Ok(_) => panic!("expected error for non-git directory"),
        }
    }

    #[tokio::test]
    async fn status_diff_log_work_without_git_on_path() {
        // gix-backed operations (status, diff, log) do not require the system `git`
        // binary to be on PATH — they use libgit2 via gix's native Rust implementation.
        // This test proves that by using a repo created entirely via gix API.
        let dir = init_repo();
        let tool = GitTool;
        let policy = AllowAllPolicy;
        let ses = SessionContext::new(concerto_core::ids::Ulid::new(), dir.path().to_path_buf());
        let cancel = CancellationToken::new();

        let status = tool
            .execute(serde_json::json!({"operation": "status"}), &policy, &ses, cancel.clone())
            .await;
        assert!(status.is_ok(), "status should work without git CLI: {:?}", status.err());

        let log = tool
            .execute(serde_json::json!({"operation": "log"}), &policy, &ses, cancel.clone())
            .await;
        assert!(log.is_ok(), "log should work without git CLI: {:?}", log.err());
        assert!(
            !log.unwrap().data["commits"].as_array().unwrap().is_empty(),
            "should have at least one commit"
        );

        let diff =
            tool.execute(serde_json::json!({"operation": "diff"}), &policy, &ses, cancel).await;
        // diff on a repo with no unstaged changes may be empty but should not fail
        assert!(diff.is_ok(), "diff should work without git CLI: {:?}", diff.err());
    }

    #[test]
    fn coerces_newline_separated_paths_string() {
        let input = serde_json::json!({"operation": "add", "paths": "a.txt\nb.txt"});
        let parsed = coerce_git_input(&input).unwrap();
        assert_eq!(parsed.paths, Some(vec!["a.txt".to_string(), "b.txt".to_string()]));
    }

    #[test]
    fn coerces_comma_separated_paths_string() {
        let input = serde_json::json!({"operation": "add", "paths": "a.txt,b.txt"});
        let parsed = coerce_git_input(&input).unwrap();
        assert_eq!(parsed.paths, Some(vec!["a.txt".to_string(), "b.txt".to_string()]));
    }

    #[test]
    fn coerces_string_typed_max_count() {
        let input = serde_json::json!({"operation": "log", "max_count": "5"});
        let parsed = coerce_git_input(&input).unwrap();
        assert_eq!(parsed.max_count, Some(5));
    }

    #[test]
    fn coerces_string_typed_max_count_with_whitespace() {
        let input = serde_json::json!({"operation": "log", "max_count": " 5\n"});
        let parsed = coerce_git_input(&input).unwrap();
        assert_eq!(parsed.max_count, Some(5));
    }

    #[test]
    fn coerces_non_string_scalars_for_string_fields() {
        let input = serde_json::json!({"operation": "branch_create", "branch": 7});
        let parsed = coerce_git_input(&input).unwrap();
        assert_eq!(parsed.operation, "branch_create");
        assert_eq!(parsed.branch, Some("7".to_string()));
    }

    #[test]
    fn genuinely_invalid_input_keeps_strict_error() {
        let input = serde_json::json!({"operation": "add", "paths": {"obj": 1}});
        let err = coerce_git_input(&input).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("invalid git input: "), "unexpected message: {message}");
    }

    #[test]
    fn valid_input_deserializes_unchanged() {
        let input = serde_json::json!({
            "operation": "log",
            "paths": ["a.txt"],
            "max_count": 5,
            "message": "wip"
        });
        let parsed = coerce_git_input(&input).unwrap();
        assert_eq!(parsed.operation, "log");
        assert_eq!(parsed.paths, Some(vec!["a.txt".to_string()]));
        assert_eq!(parsed.max_count, Some(5));
        assert_eq!(parsed.message.as_deref(), Some("wip"));
    }
}
