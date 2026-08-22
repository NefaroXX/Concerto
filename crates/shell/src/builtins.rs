use std::sync::Arc;

use async_trait::async_trait;
use camino::Utf8Path;
use concerto_core::CancellationToken;
use serde::Serialize;
use serde_json::json;
use walkdir::WalkDir;

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

        let mut entries = Vec::new();
        let mut diagnostics = Vec::new();
        let walker =
            WalkDir::new(&target).follow_links(false).max_depth(options.depth).sort_by_file_name();

        for item in walker {
            if cancel.is_cancelled() {
                return cancelled(&self.spec.name);
            }
            if entries.len() == MAX_TREE_ENTRIES {
                diagnostics.push(Diagnostic::warning(
                    "shell.tree.truncated",
                    format!("tree output was limited to {MAX_TREE_ENTRIES} entries"),
                ));
                break;
            }

            match item {
                Ok(entry) => entries.push(tree_entry(&root, &entry)),
                Err(error) => diagnostics
                    .push(Diagnostic::warning("shell.tree.entry-unavailable", error.to_string())),
            }
        }

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

fn tree_entry(root: &Utf8Path, entry: &walkdir::DirEntry) -> TreeEntry {
    let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
    let file_type = entry.file_type();
    let kind = if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "file"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "other"
    };
    let size = if file_type.is_file() {
        entry.metadata().ok().map(|metadata| metadata.len())
    } else {
        None
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
    use camino::Utf8PathBuf;

    use super::{parse_tree_options, tree_path_error, DEFAULT_TREE_DEPTH, MAX_TREE_DEPTH};
    use crate::path::PathError;
    use crate::CommandStatus;

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
}
