//! Portable file/context workflows without provider setup.
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::sync::Arc;

use concerto_core::CancellationToken;
use concerto_orchestrator::harness_service::{build_standalone_harness, ApproveInvocationSink};
use concerto_orchestrator::workspace_files::MAX_DOCUMENT_BYTES;
use serde_json::{json, Value};

const USAGE: &str = "usage: concerto [--project DIR] workspace COMMAND\n\
    list [DIR] [--json]\n\
    read PATH [--json | --raw]\n\
    context PATH ... [--json]\n\
    search QUERY PATH ... [--json]\n\
    write PATH --stdin (--expect REVISION | --create) --yes [--json]\n\
    delete PATH --expect REVISION --yes [--json]\n\
Read returns a revision for later write/delete. --raw is for redirected UTF-8\n\
export to an external editor. Apply its edited text with write --stdin; edits\n\
go through policy and audit. File size: 1 MiB; context/search: at most 32 files.";

#[derive(Debug, PartialEq, Eq)]
struct Options {
    operation: String,
    paths: Vec<String>,
    query: String,
    expected: Option<String>,
    create: bool,
    json: bool,
    raw: bool,
}

fn parse(args: &[String]) -> anyhow::Result<Options> {
    let operation = args.first().ok_or_else(|| anyhow::anyhow!(USAGE))?.clone();
    anyhow::ensure!(matches!(operation.as_str(), "list" | "read" | "context" | "search" | "write" | "delete"), "{USAGE}");
    let mut options = Options { operation, paths: Vec::new(), query: String::new(), expected: None, create: false, json: false, raw: false };
    let mut confirmed = false;
    let mut stdin = false;
    let mut literal = false;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if literal {
            options.paths.push(arg.clone());
        } else {
            match arg.as_str() {
                "--" => literal = true,
                "--json" if !options.json => options.json = true,
                "--raw" if options.operation == "read" && !options.raw => options.raw = true,
                "--yes" if matches!(options.operation.as_str(), "write" | "delete") && !confirmed => confirmed = true,
                "--stdin" if options.operation == "write" && !stdin => stdin = true,
                "--create" if options.operation == "write" && !options.create => options.create = true,
                "--expect" if matches!(options.operation.as_str(), "write" | "delete") && options.expected.is_none() => {
                    index += 1;
                    let value = args.get(index).ok_or_else(|| anyhow::anyhow!("--expect requires a revision"))?;
                    anyhow::ensure!(value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()), "revision must be the 64-character hash returned by read");
                    options.expected = Some(value.to_ascii_lowercase());
                }
                value if !value.starts_with('-') => options.paths.push(value.to_owned()),
                other => anyhow::bail!("unexpected argument '{other}'; {USAGE}"),
            }
        }
        index += 1;
    }
    anyhow::ensure!(!(options.json && options.raw), "choose --json or --raw");
    if options.operation == "search" {
        anyhow::ensure!(options.paths.len() >= 2, "search requires QUERY and files");
        options.query = options.paths.remove(0);
        anyhow::ensure!(!options.query.is_empty(), "query cannot be empty");
    }
    if options.operation == "list" && options.paths.is_empty() { options.paths.push(".".into()); }
    let multi = matches!(options.operation.as_str(), "search" | "context");
    anyhow::ensure!(!options.paths.is_empty() && options.paths.len() <= if multi { 32 } else { 1 }, "invalid file count; {USAGE}");
    if matches!(options.operation.as_str(), "write" | "delete") {
        anyhow::ensure!(confirmed, "mutation requires --yes; {USAGE}");
        anyhow::ensure!(options.create != options.expected.is_some(), "choose exactly one of --create or --expect REVISION");
    }
    anyhow::ensure!(options.operation != "write" || stdin, "write requires --stdin");
    Ok(options)
}

pub(crate) fn run(args: &[String], project: &Path) -> anyhow::Result<()> {
    if matches!(args, [arg] if matches!(arg.as_str(), "help" | "--help" | "-h")) {
        println!("{USAGE}");
        return Ok(());
    }
    let options = parse(args)?;
    anyhow::ensure!(!options.raw || !std::io::stdout().is_terminal(), "--raw requires redirected stdout; use JSON for terminal-safe inspection");
    let content = if options.operation == "write" {
        anyhow::ensure!(!std::io::stdin().is_terminal(), "redirect the edited UTF-8 buffer to stdin");
        let mut bytes = Vec::new();
        std::io::stdin().take(MAX_DOCUMENT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= MAX_DOCUMENT_BYTES, "document exceeds 1 MiB limit");
        Some(String::from_utf8(bytes)?)
    } else { None };
    let project = project.canonicalize()?;
    let config = concerto_config::load_config(None, Some(&project))?;
    let runtime = tokio::runtime::Runtime::new()?;
    let value = runtime.block_on(async {
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let listener = tokio::spawn(async move { if tokio::signal::ctrl_c().await.is_ok() { signal.cancel(); } });
        let result = async {
            let harness = tokio::select! {
                biased;
                _ = cancel.cancelled() => anyhow::bail!("cancelled"),
                result = build_standalone_harness(&project, &config, false, Arc::new(ApproveInvocationSink)) => result?,
            };
            let service = &harness.service;
            let path = &options.paths[0];
            let value = match options.operation.as_str() {
                "list" => service.workspace_list(path, cancel.clone()).await?,
                "read" => serde_json::to_value(service.workspace_read(path, cancel.clone()).await?)?,
                "search" => service.workspace_search(&options.paths, &options.query, cancel.clone()).await?,
                "context" => {
                    let mut documents = Vec::new();
                    let mut total = 0;
                    for path in &options.paths {
                        let document = service.workspace_read(path, cancel.clone()).await?;
                        total += document.content.len();
                        anyhow::ensure!(total <= 4 * MAX_DOCUMENT_BYTES, "context exceeds 4 MiB; select fewer files");
                        documents.push(document);
                    }
                    json!({"documents": documents, "bytes": total})
                }
                "write" => service.workspace_write(path, content.as_deref().unwrap_or_default(), options.expected.as_deref(), cancel.clone()).await?,
                "delete" => service.workspace_delete(path, options.expected.as_deref().unwrap_or_default(), cancel.clone()).await?,
                _ => anyhow::bail!(USAGE),
            };
            Ok::<Value, anyhow::Error>(value)
        }.await;
        listener.abort();
        result
    })?;
    if options.raw {
        let content = value["content"].as_str().ok_or_else(|| anyhow::anyhow!("read returned no text"))?;
        std::io::stdout().write_all(content.as_bytes())?;
    } else if options.json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> { values.iter().map(|value| (*value).into()).collect() }

    #[test]
    fn mutations_require_confirmation_and_one_revision_mode() {
        for values in [
            vec!["write", "a", "--stdin", "--create"],
            vec!["write", "a", "--stdin", "--yes"],
            vec!["delete", "a", "--yes"],
            vec!["delete", "a", "--create", "--yes"],
            vec!["write", "a", "--stdin", "--expect", "invalid", "--yes"],
            vec!["read", "a", "--raw", "--json"],
            vec!["search", "", "a"],
        ] { assert!(parse(&args(&values)).is_err(), "{values:?}"); }
        assert!(parse(&args(&["write", "a", "--stdin", "--create", "--yes"])).is_ok());
        let revision = "a".repeat(64);
        assert!(parse(&args(&["delete", "a", "--expect", &revision, "--yes"])).is_ok());
        assert!(parse(&args(&["write", "a", "--stdin", "--expect", &revision, "--create", "--yes"])).is_err());
    }

    #[test]
    fn literal_operands_preserve_startup_flag_names() {
        let options = parse(&args(&["search", "--json", "--", "--fast", "--project"])).unwrap();
        assert_eq!(options.query, "--fast");
        assert_eq!(options.paths, ["--project"]);
        assert!(options.json);
    }
}
