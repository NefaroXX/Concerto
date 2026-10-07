//! `concerto memory <graph|explain>` — read-only memory evidence views (ADR-69).
//!
//! `graph` renders the project's memory chunks + links as Mermaid `flowchart
//! TD` source; `explain <chunk-id>` prints one chunk's links and score. Both
//! only read `<app data>/memory/memory.db` and never modify it.

use std::path::{Path, PathBuf};

use concerto_core::CancellationToken;

/// Resolve the on-disk path of the project memory db (shared with the runtime).
fn memory_db_path() -> anyhow::Result<PathBuf> {
    let data_dir = concerto_sessions::app_data_dir()
        .map_err(|e| anyhow::anyhow!("could not resolve app data dir: {e}"))?;
    Ok(data_dir.join("memory").join("memory.db"))
}

/// Manually parse `memory graph` flags (no clap, matching the rest of the CLI).
///
/// Returns `(limit, max_nodes, max_edges)`.
fn parse_memory_graph_args(args: &[String]) -> anyhow::Result<(usize, usize, usize)> {
    let usage = "usage: concerto memory graph [--limit <n>] [--max-nodes <n>] [--max-edges <n>]";
    let mut limit = 200usize;
    let mut max_nodes = 100usize;
    let mut max_edges = 400usize;
    let mut index = 0;
    while index < args.len() {
        let value = |args: &[String], index: &mut usize| -> anyhow::Result<usize> {
            *index += 1;
            let raw = args.get(*index).ok_or_else(|| anyhow::anyhow!("missing value; {usage}"))?;
            raw.parse::<usize>().map_err(|_| anyhow::anyhow!("invalid number '{raw}'; {usage}"))
        };
        match args[index].as_str() {
            "--limit" => limit = value(args, &mut index)?,
            "--max-nodes" => max_nodes = value(args, &mut index)?,
            "--max-edges" => max_edges = value(args, &mut index)?,
            other => anyhow::bail!("unknown memory graph option '{other}'; {usage}"),
        }
        index += 1;
    }
    Ok((limit, max_nodes, max_edges))
}

/// `concerto memory <graph|explain <chunk-id>>`.
pub(crate) fn run_memory_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: concerto memory <graph|explain <chunk-id>>");
    }
    match args[0].as_str() {
        "graph" => run_memory_graph(&args[1..], project_root),
        "explain" => run_memory_explain(&args[1..], project_root),
        other => anyhow::bail!(
            "unknown memory subcommand '{other}'; usage: concerto memory <graph|explain <chunk-id>>"
        ),
    }
}

/// `concerto memory graph` — print the project's memory graph as Mermaid.
fn run_memory_graph(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    let (limit, max_nodes, max_edges) = parse_memory_graph_args(args)?;
    let db_path = memory_db_path()?;
    let project_id =
        concerto_core::memory::ProjectId(concerto_core::helpers::project_id_hash(project_root));
    let rt = tokio::runtime::Runtime::new()?;
    let graph = rt.block_on(concerto_memory::mermaid::load_memory_graph(
        &db_path,
        &project_id,
        limit,
        CancellationToken::new(),
    ))?;
    if graph.nodes.is_empty() {
        println!("No memory graph entries found for this project.");
        println!("Memory db: {}", db_path.display());
        return Ok(());
    }
    let cap = concerto_memory::mermaid::GraphCap { max_nodes, max_edges };
    println!("{}", concerto_memory::mermaid::render_mermaid(&graph, cap));
    println!("---");
    println!(
        "{} nodes, {} edges (limit {limit} chunks) — db {}",
        graph.nodes.len(),
        graph.edges.len(),
        db_path.display()
    );
    Ok(())
}

/// `concerto memory explain <chunk-id>` — print one chunk's links and score.
fn run_memory_explain(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.len() != 1 {
        anyhow::bail!("usage: concerto memory explain <chunk-id>");
    }
    let chunk_id = args[0].to_string();
    let db_path = memory_db_path()?;
    let project_id =
        concerto_core::memory::ProjectId(concerto_core::helpers::project_id_hash(project_root));
    let rt = tokio::runtime::Runtime::new()?;
    let detail = rt.block_on(concerto_memory::mermaid::load_chunk_detail(
        &db_path,
        &project_id,
        &chunk_id,
        CancellationToken::new(),
    ))?;
    let Some(detail) = detail else {
        println!("No such chunk '{chunk_id}' in this project's memory.");
        println!("Memory db: {}", db_path.display());
        return Ok(());
    };

    println!("=== Chunk {} ===", detail.chunk_id);
    println!("score: {:.2}", detail.score);
    println!("content:");
    for line in detail.content.lines().take(12) {
        println!("  {line}");
    }
    if detail.content.lines().count() > 12 {
        println!("  …");
    }
    if detail.out_links.is_empty() {
        println!("outgoing links: none");
    } else {
        println!("outgoing links:");
        for link in &detail.out_links {
            println!("  -> {} ({}, weight {})", link.to, link.kind.as_str(), link.weight);
        }
    }
    if detail.in_links.is_empty() {
        println!("incoming links: none");
    } else {
        println!("incoming links:");
        for link in &detail.in_links {
            println!("  <- {} ({}, weight {})", link.from, link.kind.as_str(), link.weight);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::ENV_LOCK;

    #[test]
    fn parse_memory_graph_args_defaults_and_overrides() {
        let (limit, nodes, edges) = parse_memory_graph_args(&[]).unwrap();
        assert_eq!((limit, nodes, edges), (200, 100, 400));

        let (limit, nodes, edges) = parse_memory_graph_args(&[
            "--limit".to_string(),
            "50".to_string(),
            "--max-nodes".to_string(),
            "10".to_string(),
            "--max-edges".to_string(),
            "20".to_string(),
        ])
        .unwrap();
        assert_eq!((limit, nodes, edges), (50, 10, 20));

        assert!(parse_memory_graph_args(&["--bogus".to_string()]).is_err());
        assert!(parse_memory_graph_args(&["--limit".to_string()]).is_err());
        assert!(parse_memory_graph_args(&["--limit".to_string(), "x".to_string()]).is_err());
    }

    #[test]
    fn run_memory_graph_missing_db_returns_ok() {
        // Point XDG_DATA_HOME at a temp dir so the read-only memory view
        // resolves an empty (non-existent) memory db and prints a friendly
        // "no entries" message instead of failing.
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", temp.path());
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let result = run_memory_subcommand(&["graph".to_string()], &project_root);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(result.is_ok(), "memory graph with missing db should succeed: {:?}", result.err());
    }

    #[test]
    fn run_memory_explain_unknown_chunk_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", temp.path());
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let result = run_memory_subcommand(
            &["explain".to_string(), "chunk-unknown".to_string()],
            &project_root,
        );

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(
            result.is_ok(),
            "memory explain with unknown chunk should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn run_memory_no_args_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        assert!(run_memory_subcommand(&[], &project_root).is_err());
    }
}
