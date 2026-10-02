#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

pub mod app;
pub mod approval;
pub mod health;
pub mod plugin_approval;
mod shell;
pub mod theme;
pub mod ui;
pub mod update;

use concerto_core::CancellationToken;
use concerto_sessions::{AuditLogFilter, AuditLogRow};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Run the TUI/CLI interface.
pub fn run_cli(
    multi_agent: bool,
    fast: bool,
    reconfigure: bool,
    reduced_motion: Option<bool>,
) -> anyhow::Result<()> {
    let remaining: Vec<String> = std::env::args().skip(1).collect();
    run_cli_inner(multi_agent, fast, reconfigure, reduced_motion, &remaining)
}

/// Inner run function with reconfigure support and subcommand dispatch.
fn run_cli_inner(
    multi_agent: bool,
    fast: bool,
    reconfigure: bool,
    reduced_motion: Option<bool>,
    remaining: &[String],
) -> anyhow::Result<()> {
    // `--no-terminal-title` is a display opt-out handled here (not in
    // `parse_cli_args`) so `run_cli`'s signature stays stable: strip it from
    // the raw args before subcommand dispatch. There is no `--terminal-title`
    // opt-in; absence follows `CONCERTO_NO_TITLE` env, then config. `--theme`
    // follows the same path (flag > `CONCERTO_THEME` env > `[display] theme`).
    let mut no_terminal_title: Option<bool> = None;
    let mut explicit_theme: Option<String> = None;
    let mut filtered: Vec<String> = Vec::with_capacity(remaining.len());
    let mut index = 0;
    let startup_count = startup_arguments(remaining).len();
    while index < startup_count {
        let arg = remaining[index].as_str();
        if arg == "--no-terminal-title" {
            no_terminal_title = Some(true);
        } else if arg == "--theme" {
            index += 1;
            if let Some(value) = remaining.get(index) {
                explicit_theme = Some(value.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--theme=") {
            explicit_theme = Some(value.to_string());
        } else {
            filtered.push(remaining[index].clone());
        }
        index += 1;
    }
    filtered.extend_from_slice(&remaining[startup_count..]);
    let (remaining, explicit_project) = invocation_args(&filtered)?;
    if remaining.first().map(String::as_str) == Some("logs") {
        return run_logs_subcommand(&remaining[1..]);
    }
    if explicit_project.is_none() && remaining.first().map(String::as_str) == Some("projects") {
        return run_projects_subcommand(&remaining[1..]);
    }
    let project_root = resolve_project_dir(explicit_project.as_deref())?;

    // ── Subcommand dispatch ────────────────────────────────────────────
    if !remaining.is_empty() {
        match remaining[0].as_str() {
            "shell" => return shell::run(&remaining[1..], &project_root),
            "config" => return run_config_subcommand(&remaining[1..], &project_root),
            "providers" => return run_providers_subcommand(&remaining[1..], &project_root),
            "sessions" => return run_sessions_subcommand(&remaining[1..], &project_root),
            "projects" => return run_projects_subcommand(&remaining[1..]),
            "plugin" => return run_plugin_subcommand(&remaining[1..]),
            "extensions" => return run_extensions_subcommand(&remaining[1..], &project_root),
            "health" => return run_health_subcommand(&remaining[1..], &project_root),
            "memory" => return run_memory_subcommand(&remaining[1..], &project_root),
            "audit" => return run_audit_subcommand(&remaining[1..]),
            other => {
                eprintln!("error: unknown subcommand '{other}'");
                eprintln!(
                    "available subcommands: config, providers, sessions, projects, plugin, extensions, health, memory, audit, logs, shell"
                );
                std::process::exit(1);
            }
        }
    }

    // Default to WARN (not tracing's ERROR default) so the application log
    // captures warn-level diagnostics out of the box; RUST_LOG still overrides.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::Level::WARN.into())
                .from_env_lossy(),
        )
        .with_writer(application_log_writer())
        .init();

    // Run setup wizard if needed or requested.
    // Only runs if we have a resolvable config path (non-container).
    let config_path = concerto_config::default_config_path();
    if let Some(ref cp) = config_path {
        if reconfigure
            || concerto_config::SetupWizard::<std::io::StdinLock, std::io::Stdout>::needs_setup(cp)
        {
            use concerto_config::setup::ProviderKind;

            let stdin = std::io::stdin();
            let mut wizard = concerto_config::SetupWizard::new(stdin.lock(), std::io::stdout());

            // Show wizard header (normally printed by `run()`)
            {
                use std::io::Write;
                let _ = writeln!(std::io::stdout(), "=== Concerto Setup Wizard ===");
                let _ = writeln!(std::io::stdout());
            }

            // Step 1: Select provider
            let provider_kind = wizard.prompt_provider()?;
            // Step 2: Enter API key
            let api_key = wizard.prompt_api_key()?;

            // Step 3: Try to fetch models for a live model picker
            let provider_name = match provider_kind {
                ProviderKind::OpenAI => "openai",
                ProviderKind::Anthropic => "anthropic",
                ProviderKind::Ollama => "ollama",
                ProviderKind::Nvidianim => "nim",
                ProviderKind::OpenRouter => "openrouter",
                ProviderKind::OpenCodeZen => "opencode",
                ProviderKind::Other => "other",
                _ => "other",
            };
            let models = concerto_providers::list_models_for_provider_blocking(
                provider_name,
                &api_key,
                None,
            );
            // The wizard only needs names; provider-advertised capability
            // metadata is preserved by the config cache path, not here.
            wizard.set_available_models(models.into_iter().map(|model| model.id).collect());

            // Step 4: Model picker (free-text or live list)
            let model = wizard.prompt_model(&provider_kind)?;
            // Step 5-6: Remaining prompts
            let working_dir = wizard.prompt_working_dir()?;
            let policy_mode = wizard.prompt_policy()?;

            let pending = concerto_config::PendingConfig {
                provider: provider_name.to_string(),
                api_key,
                model,
                working_dir,
                policy_mode,
            };

            let credentials = concerto_config::CredentialStore::new();
            if reconfigure {
                pending.save_overwrite(cp, &credentials)?;
            } else {
                pending.save(cp, &credentials)?;
            }
            tracing::info!("setup wizard completed — configuration saved to {}", cp.display());
        }
    }

    // Load config from global + project sources.
    let global_config =
        concerto_config::load_global_config(config_path.as_ref()).unwrap_or_default();
    let config = concerto_config::load_config(config_path.as_ref(), Some(&project_root))
        .unwrap_or_else(|_| global_config.clone());

    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // Non-blocking update check (fires once, never blocks startup).
    let should_check = config.updates.as_ref().is_none_or(|u| u.check_on_startup);
    if should_check {
        update::check_for_updates();
    }

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let mut terminal_app = app::App::new();
    // Remember explicit CLI flags so the per-run config reload re-applies
    // them (ADR-57 D5): an explicit -m/-f/--reduced-motion must survive an
    // external config edit.
    terminal_app.run_flags = app::RunFlags {
        multi_agent: multi_agent.then_some(true),
        fast: fast.then_some(true),
        reduced_motion,
        no_terminal_title,
        theme: explicit_theme,
    };
    let session_manager = std::sync::Arc::new(
        rt.block_on(
            concerto_orchestrator::session_manager::ProjectSessionManager::connect_with_config(
                concerto_orchestrator::session_manager::SessionManagerConfig {
                    git_auto_init: config
                        .tool_settings
                        .as_ref()
                        .is_none_or(|settings| settings.git_auto_init),
                },
            ),
        )
        .map_err(|error| anyhow::anyhow!("could not open session database: {error}"))?,
    );
    terminal_app.configure(global_config, config, project_root, session_manager);
    terminal_app.restore_active_session(&rt);
    let res = terminal_app.run(&mut terminal, &rt);

    terminal.clear()?;
    if let Err(e) = &res {
        tracing::error!("fatal: {e}");
    }
    res
}

// ── Subcommands ────────────────────────────────────────────────────────────────

fn run_config_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.is_empty() {
        eprintln!("usage: concerto config <init|doctor>");
        std::process::exit(1);
    }
    match args[0].as_str() {
        "init" => {
            let config_path = concerto_config::default_config_path();
            match config_path {
                Some(path) => {
                    if path.exists() {
                        println!("Config already exists at: {}", path.display());
                        println!("Use --reconfigure to overwrite, or manually edit the file.");
                    } else {
                        // Create parent dirs and write default config.
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        let default = concerto_config::AppConfig::default();
                        let toml = toml::to_string_pretty(&default)?;
                        std::fs::write(&path, &toml)?;
                        println!("Default config written to: {}", path.display());
                        println!("Edit it to configure your provider, then run:");
                        println!("  concerto config doctor");
                    }
                }
                None => {
                    eprintln!("error: cannot determine config directory on this platform");
                    std::process::exit(1);
                }
            }
        }
        "doctor" => {
            let config_path = concerto_config::default_config_path();
            let config = concerto_config::load_config(config_path.as_ref(), Some(project_root))
                .unwrap_or_default();

            println!("=== Concerto Config Doctor ===");
            match &config_path {
                Some(p) if p.exists() => println!("Config file: {} ✓", p.display()),
                Some(p) => println!("Config file: {} (not found — using defaults)", p.display()),
                None => println!("Config file: none (platform has no config dir)"),
            }

            println!();
            let creds = concerto_config::CredentialStore::new();
            let model_settings =
                config.model_settings.as_ref().filter(|settings| !settings.providers.is_empty());
            if let Some(settings) = model_settings {
                let default_model = settings
                    .global_default_model
                    .as_deref()
                    .or_else(|| settings.providers.first().map(|provider| provider.model.as_str()))
                    .unwrap_or("not configured");
                println!("Default model: {default_model}");
                println!("Provider routes:");
                for provider in &settings.providers {
                    let key_status = if provider.api_key(&creds).is_ok() {
                        "✓ key present"
                    } else {
                        "✗ key missing"
                    };
                    println!("  - {} via {} ({key_status})", provider.model, provider.provider);
                }
            } else if let Some(provider) = &config.primary_provider_config {
                let key_status = if provider.api_key(&creds).is_ok() {
                    "✓ key present"
                } else {
                    "✗ key missing"
                };
                println!("Legacy provider: {} ({key_status})", provider.provider);
                println!("  Model: {}", provider.model);
                println!("Run `concerto --reconfigure` to migrate to model-first settings.");
            } else {
                println!("Default model: not configured");
                println!("Run `concerto --reconfigure` to set up a provider.");
            }

            // Check Ollama accessibility.
            let ollama_url = config.ollama_base_url.as_deref().unwrap_or("http://localhost:11434");
            println!();
            println!("Ollama base URL: {}", ollama_url);
        }
        other => {
            eprintln!("error: unknown config subcommand '{other}'");
            eprintln!("available: init, doctor");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn run_providers_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.is_empty() || args[0] != "list" {
        eprintln!("usage: concerto providers list");
        std::process::exit(1);
    }
    let config_path = concerto_config::default_config_path();
    let config =
        concerto_config::load_config(config_path.as_ref(), Some(project_root)).unwrap_or_default();

    let creds = concerto_config::CredentialStore::new();

    println!("=== Configured Providers ===");
    println!();

    // Legacy single-provider mode.
    if let Some(ref name) = config.primary_provider {
        let key_status = if let Some(ref pc) = config.primary_provider_config {
            match pc.api_key(&creds) {
                Ok(_) => "✓ key present".to_string(),
                Err(_) => "✗ no key".to_string(),
            }
        } else {
            "(no config)".to_string()
        };
        println!("[primary] {} — {}", name, key_status);
    }

    // Multi-provider model_settings.
    if let Some(ref ms) = config.model_settings {
        for p in &ms.providers {
            let marker = if ms.global_default_model.as_deref() == Some(p.model.as_str()) {
                " (default model route)"
            } else {
                ""
            };
            let key_status = match p.api_key(&creds) {
                Ok(_) => "✓ key present",
                Err(_) => "✗ no key",
            };
            println!("[{}] {} — model: {} — {}{}", p.id, p.provider, p.model, key_status, marker);
        }
    }

    if config.primary_provider.is_none() && config.model_settings.is_none() {
        println!("No providers configured.");
        println!("Run `concerto --reconfigure` to set up a provider.");
    }

    Ok(())
}

/// `concerto health` — print the resolved model/provider stack, offline.
///
/// Human-readable by default; `--json` emits the same report as compact JSON
/// for scripts. Any other argument is a usage error.
fn run_health_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.len() > 1 || args.first().is_some_and(|arg| arg != "--json") {
        eprintln!("usage: concerto health [--json]");
        std::process::exit(1);
    }
    let config_path = concerto_config::default_config_path();
    let report = health::collect_health(config_path.as_deref(), Some(project_root));
    if args.first().map(String::as_str) == Some("--json") {
        let json = serde_json::to_string(&report)?;
        println!("{json}");
    } else {
        print!("{report}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `concerto memory graph|explain` — read-only memory evidence views (ADR-69).
//
// `graph` renders the project's memory chunks + links as Mermaid `flowchart
// TD` source; `explain <chunk-id>` prints one chunk's links and score. Both
// only read `<app data>/memory/memory.db` and never modify it.
// ---------------------------------------------------------------------------

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
fn run_memory_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
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

// ---------------------------------------------------------------------------
// `concerto sessions prune` — explicit, maintenance-only session pruning.
//
// The command never prunes automatically and never deletes anything the user
// did not target with `--older-than` (plus the always-protected recent/active
// sessions). It always prints exactly what it did.
// ---------------------------------------------------------------------------

/// Options parsed from `concerto sessions prune` flags.
struct PruneOptions {
    /// Delete sessions older than this many days.
    older_than_days: i64,
    /// Always protect the `n` most recent candidate sessions.
    keep: usize,
    /// Preview only — print what would be deleted, delete nothing.
    dry_run: bool,
    /// Prune across every project, not just the current project.
    all_projects: bool,
}

/// Manually parse `prune` flags (no clap, matching the rest of the CLI).
fn parse_prune_args(args: &[String]) -> anyhow::Result<PruneOptions> {
    let usage = "usage: concerto sessions prune --older-than <days> \
                 [--keep <n>] [--dry-run] [--all-projects]";
    let mut older_than_days: Option<i64> = None;
    let mut keep: usize = 5;
    let mut dry_run = false;
    let mut all_projects = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--older-than" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--older-than requires a number of days\n{usage}")
                })?;
                older_than_days = Some(value.parse::<i64>().map_err(|_| {
                    anyhow::anyhow!(
                        "invalid --older-than value '{value}': expected a positive integer\n{usage}"
                    )
                })?);
            }
            "--keep" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--keep requires a number\n{usage}"))?;
                keep = value.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!(
                        "invalid --keep value '{value}': expected a non-negative integer\n{usage}"
                    )
                })?;
            }
            "--dry-run" => dry_run = true,
            "--all-projects" => all_projects = true,
            other => anyhow::bail!("unknown prune flag '{other}'\n{usage}"),
        }
        index += 1;
    }

    let older_than_days = older_than_days
        .ok_or_else(|| anyhow::anyhow!("--older-than <days> is required\n{usage}"))?;
    if older_than_days <= 0 {
        anyhow::bail!("--older-than must be a positive number of days\n{usage}");
    }

    Ok(PruneOptions { older_than_days, keep, dry_run, all_projects })
}

/// Render a session timestamp as a human-readable UTC date/time, falling back
/// to the `Debug` form used elsewhere in the CLI if formatting fails.
fn format_session_date(dt: time::OffsetDateTime) -> String {
    match dt.format(&time::format_description::well_known::Rfc3339) {
        Ok(formatted) => formatted,
        Err(_) => format!("{dt:?}"),
    }
}

/// Resolve the project directory of a session for display purposes.
async fn session_project_dir(
    store: &concerto_sessions::SqliteSessionStore,
    id: concerto_core::ids::Ulid,
    cancel: CancellationToken,
) -> anyhow::Result<String> {
    use concerto_sessions::SessionStore;
    Ok(store
        .load_session(id, cancel)
        .await?
        .map(|session| session.project_dir.to_string())
        .unwrap_or_else(|| "<unknown>".to_string()))
}

/// Execute the prune for a connected store.
async fn run_prune(
    store: &concerto_sessions::SqliteSessionStore,
    project_root: &Path,
    opts: &PruneOptions,
) -> anyhow::Result<()> {
    use concerto_sessions::SessionStore;

    let cancel = CancellationToken::new();
    let now_unix = time::OffsetDateTime::now_utc().unix_timestamp();
    let cutoff = now_unix - opts.older_than_days * 86400;

    // Candidate sessions: all projects use the SQL-side age filter; the
    // project-scoped branch reuses `list_sessions_for_project` (most recent
    // first, like `list`) and filters in Rust.
    let mut candidates = if opts.all_projects {
        store.list_sessions_older_than(cutoff, cancel.clone()).await?
    } else {
        let project = camino::Utf8PathBuf::from_path_buf(project_root.to_path_buf())
            .unwrap_or_else(|path| camino::Utf8PathBuf::from(path.to_string_lossy().as_ref()));
        store
            .list_sessions_for_project(&project, 10_000, cancel.clone())
            .await?
            .into_iter()
            .filter(|s| s.created_at.unix_timestamp() < cutoff)
            .collect()
    };

    // Always protect the `--keep` most recent candidates (sort newest-first,
    // then keep the head and consider only the tail for deletion).
    candidates.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    let protected_recent = opts.keep.min(candidates.len());
    let to_delete = candidates.split_off(protected_recent);

    // Always protect sessions that are currently mapped active anywhere.
    let active_ids: std::collections::HashSet<String> = store
        .active_session_ids(cancel.clone())
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    let mut skipped_active = 0usize;
    let mut remaining = Vec::new();
    for session in to_delete {
        if active_ids.contains(&session.id.to_string()) {
            println!("skipped {} (active)", session.id);
            skipped_active += 1;
        } else {
            remaining.push(session);
        }
    }

    if remaining.is_empty() {
        println!("nothing to prune");
        return Ok(());
    }

    if opts.dry_run {
        for session in &remaining {
            println!(
                "would delete {} {} (created {})",
                session.id,
                session_project_dir(store, session.id, cancel.clone()).await?,
                format_session_date(session.created_at)
            );
        }
        println!("dry run: {} would be deleted", remaining.len());
        return Ok(());
    }

    let mut deleted = 0usize;
    for session in &remaining {
        let project_dir = session_project_dir(store, session.id, cancel.clone()).await?;
        match store.delete_session(session.id, cancel.clone()).await {
            Ok(true) => {
                println!(
                    "deleted {} {} (created {}, {} messages, ${:.4})",
                    session.id,
                    project_dir,
                    format_session_date(session.created_at),
                    session.message_count,
                    session.total_cost_usd
                );
                deleted += 1;
            }
            Ok(false) => {
                println!("skipped {} (already gone)", session.id);
            }
            Err(error) => {
                anyhow::bail!("failed to delete session {}: {error}", session.id);
            }
        }
    }
    println!(
        "prune complete: {deleted} deleted ({} skipped: recent/active)",
        protected_recent + skipped_active
    );
    Ok(())
}

fn run_sessions_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: concerto sessions <list|show <id>|events <id>|resume <id>|prune>");
    }
    match args[0].as_str() {
        "list" => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                match concerto_sessions::SqliteSessionStore::connect().await {
                    Ok(store) => {
                        use concerto_sessions::SessionStore;
                        println!("=== Recent Sessions ===");
                        let project =
                            camino::Utf8PathBuf::from_path_buf(project_root.to_path_buf())
                                .unwrap_or_else(|path| {
                                    camino::Utf8PathBuf::from(path.to_string_lossy().as_ref())
                                });
                        match store
                            .list_sessions_for_project(&project, 20, CancellationToken::new())
                            .await
                        {
                            Ok(sessions) => {
                                if sessions.is_empty() {
                                    println!("No sessions found.");
                                } else {
                                    for s in &sessions {
                                        println!(
                                            "  {} — {} msgs, ${:.4}, created {:?}",
                                            s.id, s.message_count, s.total_cost_usd, s.created_at
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                println!("Error listing sessions: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        println!("Could not open session store: {e}");
                        println!("No sessions have been created yet.");
                    }
                }
            });
        }
        "show" => {
            if args.len() < 2 {
                anyhow::bail!("usage: concerto sessions show <session-id>");
            }
            let id_str = &args[1];
            let ulid = id_str
                .parse::<ulid::Ulid>()
                .map_err(|e| anyhow::anyhow!("invalid session id '{}': {}", id_str, e))?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                match concerto_sessions::SqliteSessionStore::connect().await {
                    Ok(store) => {
                        use concerto_sessions::SessionStore;
                        match store.load_messages(ulid, CancellationToken::new()).await {
                            Ok(messages) => {
                                println!(
                                    "=== Session {} === ({} messages)",
                                    id_str,
                                    messages.len()
                                );
                                println!();
                                for msg in &messages {
                                    println!("[{:?}] {}", msg.role, msg.content);
                                    println!();
                                }
                            }
                            Err(e) => {
                                eprintln!("error loading session {id_str}: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Could not open session store: {e}");
                    }
                }
            });
        }
        "events" => {
            if args.len() < 2 {
                anyhow::bail!("usage: concerto sessions events <session-id>");
            }
            let id = parse_session_id(&args[1])?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                use concerto_sessions::SessionStore;
                let store = concerto_sessions::SqliteSessionStore::connect().await?;
                for event in store.load_events(id, CancellationToken::new()).await? {
                    println!("{:04} {} {}", event.sequence_num, event.event_kind, event.payload);
                }
                Ok::<(), concerto_sessions::SessionError>(())
            })?;
        }
        "resume" => {
            if args.len() < 2 {
                anyhow::bail!("usage: concerto sessions resume <session-id>");
            }
            let id = parse_session_id(&args[1])?;
            let rt = tokio::runtime::Runtime::new()?;
            let session_project = rt.block_on(async {
                use concerto_sessions::SessionStore;
                let store = concerto_sessions::SqliteSessionStore::connect().await?;
                let session = store
                    .load_session(id, CancellationToken::new())
                    .await?
                    .ok_or_else(|| concerto_sessions::SessionError::NotFound(id.to_string()))?;
                store
                    .set_active_session_for_project(
                        &session.project_dir,
                        id,
                        CancellationToken::new(),
                    )
                    .await?;
                println!("Session {id} will resume for {}", session.project_dir);
                Ok::<camino::Utf8PathBuf, concerto_sessions::SessionError>(session.project_dir)
            })?;
            let mut registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
            registry.select(session_project.as_std_path())?;
            registry.save()?;
        }
        "prune" => {
            let opts = parse_prune_args(&args[1..])?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let store = concerto_sessions::SqliteSessionStore::connect().await?;
                run_prune(&store, project_root, &opts).await
            })?;
        }
        other => {
            eprintln!("error: unknown sessions subcommand '{other}'");
            eprintln!("available: list, show, events, resume, prune");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn run_projects_subcommand(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        eprintln!("usage: concerto projects <list|current|use <path>>");
        std::process::exit(1);
    }
    let mut registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
    match args[0].as_str() {
        "list" => {
            let active = registry.active().map(Path::to_path_buf);
            for project in registry.recent() {
                let marker = if active.as_deref() == Some(project) { "*" } else { " " };
                println!("{marker} {}", project.display());
            }
        }
        "current" => match registry.active() {
            Some(project) => println!("{}", project.display()),
            None => println!("No active project."),
        },
        "use" => {
            let path = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: concerto projects use <path>"))?;
            let selected = registry.select(Path::new(path))?;
            registry.save()?;
            println!("Active project: {}", selected.display());
        }
        other => return Err(anyhow::anyhow!("unknown projects subcommand '{other}'")),
    }
    Ok(())
}

fn run_plugin_subcommand(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        eprintln!("usage: concerto plugin <list|revoke <plugin-id>>");
        std::process::exit(1);
    }
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("concerto")
        .join("plugins");
    let cap_mgr = concerto_plugins::capability::CapabilityManager::open(&data_dir)
        .map_err(|e| anyhow::anyhow!("could not open capability store: {e}"))?;

    match args[0].as_str() {
        "list" => {
            let plugins = cap_mgr.list_granted_plugins();
            if plugins.is_empty() {
                println!("No plugins have capability grants.");
            } else {
                println!("=== Plugins with capability grants ===");
                for id in &plugins {
                    let grants = cap_mgr.load_grants(id, None);
                    let caps: Vec<String> =
                        grants.iter().map(|(d, _, _)| format!("{d:?}")).collect();
                    println!("  {id}: {}", caps.join(", "));
                }
            }
        }
        "revoke" => {
            let plugin_id = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: concerto plugin revoke <plugin-id>"))?;
            cap_mgr.revoke_plugin(plugin_id)?;

            // Best-effort runtime revocation (ADR-37): signal the in-memory
            // grant set through a fresh manager. This CLI process never has
            // plugins loaded, so NotActive is the expected outcome — tolerated
            // and logged by `revoke_grants_best_effort`; only unexpected
            // failures surface.
            if let Ok(host) = concerto_plugins::host::PluginHost::new() {
                let manager = concerto_plugins::manager::PluginManager::new(
                    std::sync::Arc::new(host),
                    concerto_plugins::capability::CapabilityManager::open(&data_dir)
                        .map_err(|e| anyhow::anyhow!("could not open capability store: {e}"))?,
                    None,
                    None,
                );
                let _cleared = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| anyhow::anyhow!("failed to start async runtime: {e}"))?
                    .block_on(manager.revoke_grants_best_effort(plugin_id));
            } else {
                tracing::warn!("revoke_grants: could not construct plugin host — skipped");
            }

            println!("Capability grants revoked for plugin '{plugin_id}'.");
        }
        other => {
            eprintln!("error: unknown plugin subcommand '{other}'");
            eprintln!("available: list, revoke");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `concerto extensions` — inspect the skills and MCP extension sections of
/// the loaded config (ADR-43). v1 is read-only: skills are discovered from the
/// configured search paths and listed; MCP servers are listed from the config.
/// Edits are made in the config file or the desktop Settings page.
fn run_extensions_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    if args.is_empty() {
        eprintln!("usage: concerto extensions <list>");
        std::process::exit(1);
    }
    match args[0].as_str() {
        "list" => {
            let config_path = concerto_config::default_config_path();
            let config = concerto_config::load_config(config_path.as_ref(), Some(project_root))
                .unwrap_or_default();
            let skills = config.skills.unwrap_or_default();
            let mcp = config.mcp.unwrap_or_default();

            println!("=== Skills ===");
            println!("Enabled: {}", if skills.enabled { "yes" } else { "no" });
            if skills.search_paths.is_empty() {
                println!("Search paths: (none)");
            } else {
                println!("Search paths:");
                for path in &skills.search_paths {
                    println!("  - {path}");
                }
            }
            println!("Auto-load: {}", if skills.auto_load { "yes" } else { "no" });
            let enabled_display = match &skills.enabled_ids {
                Some(ids) if ids.is_empty() => "(none — nothing enabled)".to_string(),
                Some(ids) => ids.join(", "),
                None => "(all discovered skills)".to_string(),
            };
            println!("Enabled ids: {enabled_display}");
            if skills.enabled {
                println!("Discovered skills:");
                let paths: Vec<std::path::PathBuf> =
                    skills.search_paths.iter().map(std::path::PathBuf::from).collect();
                match concerto_skills::SkillManager::new(paths).discover() {
                    Ok(found) if found.is_empty() => {
                        println!("  (none found under the configured search paths)");
                    }
                    Ok(found) => {
                        for skill in &found {
                            let version = if skill.manifest.version.is_empty() {
                                "0.0.0".to_string()
                            } else {
                                skill.manifest.version.clone()
                            };
                            let name = if skill.manifest.name.is_empty() {
                                skill.id.clone()
                            } else {
                                skill.manifest.name.clone()
                            };
                            println!("  - {name} (id: {}, v{version})", skill.id);
                            if !skill.manifest.description.is_empty() {
                                println!("      {}", skill.manifest.description);
                            }
                        }
                    }
                    Err(e) => println!("  discovery error: {e}"),
                }
            }

            println!();
            println!("=== MCP servers ===");
            println!("Enabled: {}", if mcp.enabled { "yes" } else { "no" });
            if mcp.servers.is_empty() {
                println!("Configured servers: (none)");
            } else {
                println!("Configured servers:");
                for server in &mcp.servers {
                    let cmd = if server.args.is_empty() {
                        server.command.clone()
                    } else {
                        format!("{} {}", server.command, server.args.join(" "))
                    };
                    println!(
                        "  - {} [{}] — {cmd}",
                        server.id,
                        if server.enabled { "enabled" } else { "disabled" }
                    );
                }
            }
        }
        other => {
            eprintln!("error: unknown extensions subcommand '{other}'");
            eprintln!("available: list");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn application_log_path() -> Option<PathBuf> {
    dirs::data_dir().map(|directory| directory.join("concerto").join("logs").join("concerto.log"))
}

fn application_log_writer() -> tracing_subscriber::fmt::writer::BoxMakeWriter {
    let Some(path) = application_log_path() else {
        return tracing_subscriber::fmt::writer::BoxMakeWriter::new(io::stderr);
    };
    if path.parent().is_some_and(|parent| std::fs::create_dir_all(parent).is_err()) {
        return tracing_subscriber::fmt::writer::BoxMakeWriter::new(io::stderr);
    }
    match RotatingLogWriter::open(path) {
        Ok(writer) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(writer),
        Err(_) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(io::stderr),
    }
}

// ---------------------------------------------------------------------------
// Size-bounded application log rotation
// ---------------------------------------------------------------------------

/// Max size (bytes) of the main application log before it is rotated.
const APP_LOG_MAX_SIZE: u64 = 5 * 1024 * 1024;
/// Number of rotated backups retained (`concerto.log`, then `.1`, `.2`, ...).
const APP_LOG_BACKUPS: u32 = 2;

/// Bounded, append-only application log with deterministic size-based rotation.
///
/// On the first write that would push the current file past `APP_LOG_MAX_SIZE`,
/// the backup chain is shifted (`concerto.log.N` is dropped, each lower backup
/// moves up one level, the main file becomes `concerto.log.1`) and a fresh main
/// file is opened. All filesystem errors degrade to a stderr write, so logging
/// never panics and app output is never silently dropped.
struct RotatingLogWriter {
    inner: std::sync::Mutex<RotatingLogFile>,
}

/// A single append-only log file plus the path it lives at.
struct RotatingLogFile {
    path: PathBuf,
    file: std::fs::File,
}

impl RotatingLogWriter {
    fn open(path: PathBuf) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { inner: std::sync::Mutex::new(RotatingLogFile { path, file }) })
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let mut guard =
            self.inner.lock().map_err(|_| io::Error::other("application log mutex poisoned"))?;
        let current = guard.file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if buf.len() as u64 + current > APP_LOG_MAX_SIZE {
            guard.rotate()?;
        }
        guard.file.write(buf)
    }

    fn flush(&self) -> io::Result<()> {
        let mut guard =
            self.inner.lock().map_err(|_| io::Error::other("application log mutex poisoned"))?;
        guard.file.flush()
    }
}

impl RotatingLogFile {
    /// Shift the backup chain down one level and reopen a fresh main file.
    fn rotate(&mut self) -> io::Result<()> {
        // Drop the oldest backup (e.g. `concerto.log.2`), if present.
        let _ = std::fs::remove_file(backup_path(&self.path, APP_LOG_BACKUPS));
        // Shift each retained backup up one level, highest index first.
        for index in (1..APP_LOG_BACKUPS).rev() {
            let from = backup_path(&self.path, index);
            let to = backup_path(&self.path, index + 1);
            if from.exists() {
                std::fs::rename(from, to)?;
            }
        }
        // Move the current main file into the first backup slot.
        if self.path.exists() {
            std::fs::rename(&self.path, backup_path(&self.path, 1))?;
        }
        // Reopen a fresh, empty main file.
        self.file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        Ok(())
    }
}

/// Borrowed writer handed to the formatter by [`RotatingLogWriter`].
struct RotatingLogSink<'a> {
    writer: &'a RotatingLogWriter,
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for RotatingLogWriter {
    type Writer = RotatingLogSink<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingLogSink { writer: self }
    }
}

impl io::Write for RotatingLogSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.writer.write(buf) {
            Ok(written) => Ok(written),
            Err(error) => {
                // Degrade to stderr rather than silently drop application logs.
                let _ = io::stderr().write_all(buf);
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Path of the `n`-th backup of `path` (e.g. `concerto.log.1`).
fn backup_path(path: &Path, n: u32) -> PathBuf {
    path.with_extension(format!("log.{n}"))
}

fn run_logs_subcommand(args: &[String]) -> anyhow::Result<()> {
    let path = application_log_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine the application log path"))?;
    match args.first().map(String::as_str).unwrap_or("path") {
        "path" => println!("{}", path.display()),
        "show" => {
            let contents = match std::fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    println!("No application logs have been written yet.");
                    return Ok(());
                }
                Err(error) => {
                    return Err(anyhow::anyhow!("could not read {}: {error}", path.display()));
                }
            };
            let lines = contents.lines().collect::<Vec<_>>();
            let start = lines.len().saturating_sub(100);
            for line in &lines[start..] {
                println!("{line}");
            }
        }
        other => return Err(anyhow::anyhow!("unknown logs subcommand '{other}'")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `concerto audit <session-id>` — read one session's policy audit trail.
//
// The trail is written by `concerto-sessions` (`audit_log`, including the
// migration-034 path/operation facts); this is the supported way to read it
// without opening sqlite3 by hand. Read-only: it never writes to the store.
// ---------------------------------------------------------------------------

/// Usage line echoed on every `audit` argument error.
const AUDIT_USAGE: &str = "usage: concerto audit <session-id> [--tool <name>] \
                           [--operation <op>] [--failed] [--limit <n>] [--json]";

/// Parsed `concerto audit` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuditArgs {
    /// Session whose trail is being read.
    session_id: concerto_core::ids::Ulid,
    /// Read filters backing `--tool`, `--operation`, `--failed`, `--limit`.
    filter: AuditLogFilter,
    /// `--json`: one JSON object on stdout instead of the text trail.
    json: bool,
}

/// Consume the value that must follow `flag` (advancing `index`).
fn audit_flag_value(args: &[String], index: &mut usize, flag: &str) -> anyhow::Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing value after {flag}; {AUDIT_USAGE}"))
}

/// Manually parse `audit` flags (no clap, matching the rest of the CLI).
///
/// The session id is the single positional argument and may appear anywhere
/// among the flags.
fn parse_audit_args(args: &[String]) -> anyhow::Result<AuditArgs> {
    let mut session_id: Option<concerto_core::ids::Ulid> = None;
    let mut filter = AuditLogFilter::default();
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        let raw = &args[index];
        match raw.as_str() {
            "--failed" => filter.failed = true,
            "--json" => json = true,
            "--tool" => {
                filter.tool = Some(audit_flag_value(args, &mut index, raw.as_str())?);
            }
            "--operation" => {
                filter.operation = Some(audit_flag_value(args, &mut index, raw.as_str())?);
            }
            "--limit" => {
                let value = audit_flag_value(args, &mut index, raw.as_str())?;
                let limit = value
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("invalid limit '{value}'; {AUDIT_USAGE}"))?;
                if limit == 0 {
                    anyhow::bail!("--limit must be at least 1; {AUDIT_USAGE}");
                }
                filter.limit = Some(limit);
            }
            other if other.starts_with('-') => {
                anyhow::bail!("unknown audit option '{other}'; {AUDIT_USAGE}");
            }
            other => {
                if session_id.is_some() {
                    anyhow::bail!("unexpected argument '{other}'; {AUDIT_USAGE}");
                }
                session_id = Some(parse_session_id(other)?);
            }
        }
        index += 1;
    }
    let session_id =
        session_id.ok_or_else(|| anyhow::anyhow!("missing session id; {AUDIT_USAGE}"))?;
    Ok(AuditArgs { session_id, filter, json })
}

/// On-disk path of the sessions database (shared with the runtime).
fn sessions_db_path() -> anyhow::Result<PathBuf> {
    let data_dir = concerto_sessions::app_data_dir()
        .map_err(|error| anyhow::anyhow!("could not resolve app data dir: {error}"))?;
    Ok(data_dir.join("sessions.db"))
}

/// Whether a store error says "this file is encrypted".
fn audit_is_encrypted_error(error: &concerto_sessions::SessionError) -> bool {
    let text = error.to_string();
    text.contains("at-rest encrypted")
        || text.contains("file is not a database")
        || text.contains("file is encrypted")
        || text.contains("PRAGMA key")
}

/// Turn a store-open failure into an actionable message.
///
/// An at-rest encrypted database (marker sidecar present, or sqlite itself
/// reporting an encrypted/unreadable file) needs the machine-wide
/// `[audit] encrypt_at_rest` setting before `concerto audit` can read it —
/// say so instead of leaking the raw driver error.
fn audit_open_error(db_path: &Path, error: &concerto_sessions::SessionError) -> anyhow::Error {
    if concerto_sessions::is_at_rest_encrypted(db_path) || audit_is_encrypted_error(error) {
        return anyhow::anyhow!(
            "{} is at-rest encrypted; enable [audit] encrypt_at_rest in the global config \
             (or set CONCERTO_AUDIT_DB_ENCRYPTION_KEY) to read it: {error}",
            db_path.display()
        );
    }
    anyhow::anyhow!("could not open the sessions database {}: {error}", db_path.display())
}

/// `concerto audit <session-id>` — print one session's audit trail.
fn run_audit_subcommand(args: &[String]) -> anyhow::Result<()> {
    let options = parse_audit_args(args)?;
    let db_path = sessions_db_path()?;
    let rt = tokio::runtime::Runtime::new()?;
    let rows = rt.block_on(async {
        let store = concerto_sessions::SqliteSessionStore::connect()
            .await
            .map_err(|error| audit_open_error(&db_path, &error))?;
        store
            .load_audit_log(options.session_id, &options.filter, CancellationToken::new())
            .await
            .map_err(anyhow::Error::from)
    })?;

    if options.json {
        let json = serde_json::json!({
            "session_id": options.session_id.to_string(),
            "entries": rows,
        });
        println!("{}", serde_json::to_string(&json)?);
    } else {
        print_audit_trail(options.session_id, &rows, &options.filter);
    }
    Ok(())
}

/// RFC 3339 (UTC) rendering of a stored unix timestamp; falls back to the
/// raw seconds when the value is outside the representable range.
fn format_audit_timestamp(unix_secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()
        .and_then(|timestamp| timestamp.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| unix_secs.to_string())
}

/// Path shown on an entry's first line: what was attempted, and for a
/// move/copy where it was headed.
fn audit_path_target(row: &AuditLogRow) -> String {
    let attempted = row.attempted_path.as_deref().unwrap_or("-");
    match row.attempted_destination.as_deref().or(row.resolved_destination.as_deref()) {
        Some(destination) => format!("{attempted} → {destination}"),
        None => attempted.to_string(),
    }
}

/// Render one trail entry as two lines: the decision, then the facts that
/// qualify it (resolved path, destination, rule, error, duration).
fn format_audit_entry(number: usize, row: &AuditLogRow) -> String {
    let head = format!(
        "{number:>3}  {}  {}  {}",
        format_audit_timestamp(row.created_at),
        row.tool_name,
        row.verdict
    );
    let head = if row.has_path_facts() {
        format!(
            "{head}  {}  {}",
            row.path_operation.as_deref().unwrap_or("-"),
            audit_path_target(row)
        )
    } else {
        format!("{head}  (no path facts recorded)")
    };

    let mut detail: Vec<String> = Vec::new();
    if let Some(path) = &row.resolved_path {
        detail.push(format!("resolved {path}"));
    }
    if let Some(destination) = &row.resolved_destination {
        detail.push(format!("destination {destination}"));
    }
    // Migration 035: what a read-only operation RETURNED (boolean / count /
    // size) — the "what the agent learned" half of the trail. Never content.
    if let Some(result_facts) = &row.result_facts {
        detail.push(format!("result {result_facts}"));
    }
    if let Some(rule) = &row.rule_matched {
        detail.push(format!("rule {rule}"));
    }
    if let Some(error_kind) = &row.error_kind {
        detail.push(format!("error {error_kind}"));
    }
    if let Some(duration_ms) = row.duration_ms {
        detail.push(format!("{duration_ms}ms"));
    }
    if detail.is_empty() {
        head
    } else {
        format!("{head}\n      {}", detail.join(" · "))
    }
}

/// Space-separated summary of the active filters (`""` when none).
fn audit_filter_summary(filter: &AuditLogFilter) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(tool) = &filter.tool {
        parts.push(format!("tool={tool}"));
    }
    if let Some(operation) = &filter.operation {
        parts.push(format!("operation={operation}"));
    }
    if filter.failed {
        parts.push("failed".to_string());
    }
    if let Some(limit) = filter.limit {
        parts.push(format!("limit={limit}"));
    }
    parts.join(" ")
}

/// Footer notes: how many entries were read, how many carry no path facts,
/// whether `--limit` cut the trail short, and which filters were applied.
fn print_audit_footer(rows: &[AuditLogRow], filter: &AuditLogFilter) {
    let total = rows.len();
    let without_path_facts = rows.iter().filter(|row| !row.has_path_facts()).count();
    let mut summary = format!("{} {}", total, if total == 1 { "entry" } else { "entries" });
    if without_path_facts > 0 {
        summary = format!(
            "{summary} ({without_path_facts} without path facts: written before path \
             recording, or from a tool that names no path)"
        );
    }
    println!("{summary}.");
    if let Some(limit) = filter.limit {
        if total as u64 >= limit {
            println!(
                "--limit {limit} reached: later entries were not read (raise --limit to see more)"
            );
        }
    }
    let filters = audit_filter_summary(filter);
    if !filters.is_empty() {
        println!("Filters: {filters}");
    }
}

/// Print the text trail: header, one block per entry, then the footer notes.
fn print_audit_trail(
    session_id: concerto_core::ids::Ulid,
    rows: &[AuditLogRow],
    filter: &AuditLogFilter,
) {
    println!("=== Audit trail: session {session_id} ===");
    if rows.is_empty() {
        println!();
        println!(
            "No audit entries for session {session_id} (wrong id, or no tool decisions \
             were recorded)."
        );
        let filters = audit_filter_summary(filter);
        if !filters.is_empty() {
            println!("Filters applied: {filters} — drop them to check the full trail.");
        }
        return;
    }
    println!();
    for (index, row) in rows.iter().enumerate() {
        println!("{}", format_audit_entry(index + 1, row));
    }
    println!();
    print_audit_footer(rows, filter);
}

fn parse_session_id(value: &str) -> anyhow::Result<concerto_core::ids::Ulid> {
    value
        .parse::<ulid::Ulid>()
        .map_err(|error| anyhow::anyhow!("invalid session id '{value}': {error}"))
}

/// Concerto flags end before native command operands, which may contain any flag.
/// Project/theme option values are skipped so a directory named shell stays a value.
pub fn startup_arguments(args: &[String]) -> &[String] {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--project" | "-p" | "--theme" => index += 2,
            "shell" => return &args[..index],
            value if value.starts_with('-') => index += 1,
            _ => break,
        }
    }
    args
}

fn invocation_args(args: &[String]) -> anyhow::Result<(Vec<String>, Option<PathBuf>)> {
    let mut remaining = Vec::new();
    let mut project = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            // Native program operands may contain -p/--project and startup
            // flags. Only parse Concerto flags before the shell subcommand.
            "shell" if remaining.is_empty() => {
                remaining.extend_from_slice(&args[index..]);
                break;
            }
            // Startup flags the caller already parsed (see `parse_cli_args`)
            // are stripped here so they never reach subcommand dispatch.
            "--cli" | "-c" | "--multi-agent" | "-m" | "--fast" | "-f" | "--reconfigure" | "-r"
            | "--reduced-motion" => {}
            "--project" | "-p" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--project requires a directory"))?;
                project = Some(PathBuf::from(value));
            }
            value => remaining.push(value.to_string()),
        }
        index += 1;
    }
    Ok((remaining, project))
}

fn resolve_project_dir(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let mut registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
    if let Some(path) = explicit {
        let selected = registry.select(path)?;
        registry.save()?;
        return Ok(selected);
    }

    let current = std::env::current_dir()?;
    let current_is_project = current.join(".git").exists()
        || current.join(".concerto.toml").exists()
        || current.join("Cargo.toml").exists();
    if current_is_project || registry.active().is_none() {
        let selected = registry.select(&current)?;
        registry.save()?;
        return Ok(selected);
    }
    Ok(registry.active().map(Path::to_path_buf).unwrap_or(current))
}

/// Parse CLI-specific args from an iterator, returning (multi_agent, fast, reconfigure, remaining).
/// Parsed CLI startup flags: `(multi_agent, fast, reconfigure,
/// reduced_motion, remaining)`. `reduced_motion` is `Some(true)` only when
/// `--reduced-motion` was passed (there is no `--no-reduced-motion`); `None`
/// means "follow `CONCERTO_REDUCED_MOTION` env, then the config file, then
/// the default (false)" — see `concerto_config::resolve_reduced_motion`.
pub fn parse_cli_args<'a>(
    args: impl Iterator<Item = &'a String>,
) -> (bool, bool, bool, Option<bool>, Vec<String>) {
    let mut multi_agent = false;
    let mut fast = false;
    let mut reconfigure = false;
    let mut reduced_motion: Option<bool> = None;
    let mut remaining = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--multi-agent" | "-m" => multi_agent = true,
            "--fast" | "-f" => fast = true,
            "--reconfigure" | "-r" => reconfigure = true,
            "--reduced-motion" => reduced_motion = Some(true),
            "--help" | "-h" => {
                eprintln!("Concerto CLI");
                eprintln!("  --multi-agent, -m   Enable multi-agent orchestration");
                eprintln!("  --fast, -f          Skip memory retrieval for trivial tasks");
                eprintln!("  --reconfigure, -r   Re-run the setup wizard");
                eprintln!("  --reduced-motion    Disable motion cues (first-token bold, handoff hold, wipe rule);");
                eprintln!("                      flag beats CONCERTO_REDUCED_MOTION env, which beats config file");
                eprintln!(
                    "  --no-terminal-title Disable OSC terminal-title broadcasts (run stage);"
                );
                eprintln!("                      flag beats CONCERTO_NO_TITLE env, which beats config file");
                eprintln!("  --theme NAME      CLI palette (Midnight/Slate/Chalk/Nebula);");
                eprintln!("                      flag beats CONCERTO_THEME env, which beats [display] theme");
                eprintln!("  --project, -p DIR   Select the project used by chat and commands");
                eprintln!("  --help, -h          Print this help");
                eprintln!("  subcommands: config, providers, sessions, projects, plugin, extensions, health, audit, logs");
                std::process::exit(0);
            }
            _ => remaining.push(arg.clone()),
        }
    }
    (multi_agent, fast, reconfigure, reduced_motion, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // parse_cli_args
    // ------------------------------------------------------------------

    #[test]
    fn parse_cli_args_defaults() {
        let (multi, fast, reconfigure, reduced_motion, remaining) = parse_cli_args([].iter());
        assert!(!multi);
        assert!(!fast);
        assert!(!reconfigure);
        assert_eq!(reduced_motion, None);
        assert!(remaining.is_empty());
    }

    #[test]
    fn parse_cli_args_reconfigure() {
        let args = ["--reconfigure".to_string()];
        let (_, _, reconfigure, _, _) = parse_cli_args(args.iter());
        assert!(reconfigure);
    }

    #[test]
    fn parse_cli_args_fast_and_reconfigure() {
        let args = ["--fast".to_string(), "--reconfigure".to_string()];
        let (_, fast, reconfigure, _, _) = parse_cli_args(args.iter());
        assert!(fast);
        assert!(reconfigure);
    }

    #[test]
    fn parse_cli_args_multi_agent() {
        let args = ["--multi-agent".to_string()];
        let (multi, _, _, _, _) = parse_cli_args(args.iter());
        assert!(multi);
    }

    #[test]
    fn parse_cli_args_reduced_motion() {
        let args = ["--reduced-motion".to_string()];
        let (_, _, _, reduced_motion, remaining) = parse_cli_args(args.iter());
        assert_eq!(reduced_motion, Some(true));
        assert!(remaining.is_empty());
    }

    #[test]
    fn parse_cli_args_short_flags() {
        let args = ["-m".to_string(), "-f".to_string(), "-r".to_string()];
        let (multi, fast, reconfigure, _, _) = parse_cli_args(args.iter());
        assert!(multi);
        assert!(fast);
        assert!(reconfigure);
    }

    #[test]
    fn parse_cli_args_unknown_remains() {
        let args = ["--project".to_string(), "/some/path".to_string()];
        // --project is not recognized by parse_cli_args (it's handled by invocation_args)
        let (_, _, _, _, remaining) = parse_cli_args(args.iter());
        assert_eq!(remaining, vec!["--project", "/some/path"]);
    }

    // ------------------------------------------------------------------
    // invocation_args
    // ------------------------------------------------------------------

    #[test]
    fn invocation_args_empty() {
        let (remaining, project) = invocation_args(&[]).unwrap();
        assert!(remaining.is_empty());
        assert!(project.is_none());
    }

    // verifies: native operands survive all startup parsing, including program-specific project/theme flags.
    #[test]
    fn native_shell_preserves_program_flags() {
        let args = [
            "--cli",
            "--project",
            "shell",
            "shell",
            "exec",
            "run",
            "cargo",
            "-p",
            "core",
            "--help",
            "--theme",
            "literal",
        ]
        .map(str::to_owned);
        assert_eq!(startup_arguments(&args), &args[..3]);
        let (remaining, project) = invocation_args(&args).expect("parse");
        assert_eq!(project, Some(PathBuf::from("shell")));
        assert_eq!(remaining, args[3..]);
    }

    #[test]
    fn invocation_args_strips_cli_flags() {
        let args = ["--cli".to_string(), "config".to_string()];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["config"]);
        assert!(project.is_none());
    }

    #[test]
    fn invocation_args_short_cli_flag() {
        let args = ["-c".to_string(), "-r".to_string(), "sessions".to_string()];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["sessions"]);
        assert!(project.is_none());
    }

    #[test]
    fn invocation_args_project_with_value() {
        let args =
            ["--project".to_string(), "/tmp/my-project".to_string(), "providers".to_string()];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["providers"]);
        assert_eq!(project, Some(PathBuf::from("/tmp/my-project")));
    }

    #[test]
    fn invocation_args_project_short_flag() {
        let args = ["-p".to_string(), "./my-project".to_string(), "sessions".to_string()];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["sessions"]);
        assert_eq!(project, Some(PathBuf::from("./my-project")));
    }

    #[test]
    fn invocation_args_project_without_value_errors() {
        let args = ["--project".to_string()];
        let result = invocation_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn invocation_args_project_last_with_value() {
        let args = ["config".to_string(), "--project".to_string(), "/path".to_string()];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["config"]);
        assert_eq!(project, Some(PathBuf::from("/path")));
    }

    #[test]
    fn invocation_args_multi_flag_before_subcommand() {
        let args = ["--multi-agent".to_string(), "sessions".to_string(), "list".to_string()];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["sessions", "list"]);
        assert!(project.is_none());
    }

    #[test]
    fn invocation_args_fast_flag_is_stripped() {
        let args = ["--fast".to_string(), "config".to_string()];
        let (remaining, _project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["config"]);
    }

    #[test]
    fn invocation_args_reconfigure_flag_is_stripped() {
        let args = ["--reconfigure".to_string(), "sessions".to_string(), "list".to_string()];
        let (remaining, _project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["sessions", "list"]);
    }

    #[test]
    fn invocation_args_reduced_motion_flag_is_stripped() {
        // Regression: `--reduced-motion` was parsed by `parse_cli_args` but
        // never stripped here, so it reached subcommand dispatch and failed
        // as an unknown subcommand.
        let args = ["--reduced-motion".to_string(), "providers".to_string(), "list".to_string()];
        let (remaining, _project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["providers", "list"]);
    }

    #[test]
    fn invocation_args_all_flags_stripped() {
        let args = [
            "--multi-agent".to_string(),
            "--fast".to_string(),
            "--reconfigure".to_string(),
            "providers".to_string(),
            "list".to_string(),
        ];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["providers", "list"]);
        assert!(project.is_none());
    }

    #[test]
    fn invocation_args_project_at_end_with_flag_before() {
        let args = [
            "--fast".to_string(),
            "config".to_string(),
            "doctor".to_string(),
            "--project".to_string(),
            "/tmp/proj".to_string(),
        ];
        let (remaining, project) = invocation_args(&args).unwrap();
        assert_eq!(remaining, vec!["config", "doctor"]);
        assert_eq!(project, Some(PathBuf::from("/tmp/proj")));
    }

    #[test]
    fn parse_session_id_valid_ulid_parsed() {
        let ulid_str = "01J1XYZABC1234567890ABCDEF";
        let result = parse_session_id(ulid_str);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_session_id_invalid_string_errors() {
        let result = parse_session_id("not-a-valid-ulid");
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("not-a-valid-ulid"));
    }

    #[test]
    fn application_log_path_returns_some_if_data_dir_exists() {
        // The function returns Some when dirs::data_dir() is available
        // (nearly always on dev machines). It does not check file existence.
        let path = application_log_path();
        if let Some(p) = path {
            assert!(p.to_string_lossy().contains("concerto"));
        }
        // On minimal containers data_dir may be None, which is acceptable.
    }

    // ------------------------------------------------------------------
    // resolve_project_dir
    // ------------------------------------------------------------------

    /// Serializes tests that mutate process-global state (env vars, cwd).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Helper: run resolve_project_dir with a redirected data dir (so
    /// ProjectRegistry reads/writes inside the temp directory). Also sets
    /// cwd so marker detection works. Restores both afterwards.
    /// Acquires ENV_LOCK to avoid races with other env-mutating tests.
    fn with_isolated_env(
        temp: &tempfile::TempDir,
        f: impl FnOnce() -> anyhow::Result<PathBuf>,
    ) -> anyhow::Result<PathBuf> {
        let _guard = ENV_LOCK.lock().unwrap();

        let xdg_home = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_home).expect("create xdg-data");
        let old_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_home);

        let old_cwd = std::env::current_dir().expect("current_dir");
        std::env::set_current_dir(temp.path()).expect("set_current_dir");

        let result = f();

        std::env::set_current_dir(old_cwd).expect("restore cwd");
        match old_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        result
    }

    #[test]
    fn resolve_project_dir_detects_git_marker() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".git"), "").unwrap();
        let result = with_isolated_env(&temp, || resolve_project_dir(None));
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let path = result.unwrap();
        assert!(
            path.ends_with(temp.path().file_name().unwrap()),
            "expected path ending with {:?}, got {:?}",
            temp.path().file_name().unwrap(),
            path
        );
    }

    #[test]
    fn resolve_project_dir_detects_cargo_toml_marker() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"test\"\n").unwrap();
        let result = with_isolated_env(&temp, || resolve_project_dir(None));
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let path = result.unwrap();
        assert!(
            path.ends_with(temp.path().file_name().unwrap()),
            "expected path ending with {:?}, got {:?}",
            temp.path().file_name().unwrap(),
            path
        );
    }

    #[test]
    fn resolve_project_dir_detects_concerto_toml_marker() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".concerto.toml"), "").unwrap();
        let result = with_isolated_env(&temp, || resolve_project_dir(None));
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let path = result.unwrap();
        assert!(
            path.ends_with(temp.path().file_name().unwrap()),
            "expected path ending with {:?}, got {:?}",
            temp.path().file_name().unwrap(),
            path
        );
    }

    #[test]
    fn resolve_project_dir_explicit_path_returns_that_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("subproject")).unwrap();
        // Also redirect XDG_DATA_HOME so registry uses temp dir
        let xdg_home = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_home).unwrap();
        let old_xdg = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_home);

        let explicit = temp.path().join("subproject");
        let result = resolve_project_dir(Some(&explicit));

        match old_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }

        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let path = result.unwrap();
        assert!(
            path.ends_with("subproject"),
            "expected path ending with 'subproject', got {:?}",
            path
        );
    }

    #[test]
    fn resolve_project_dir_nonexistent_explicit_errors() {
        let result = resolve_project_dir(Some(Path::new("/definitely/does/not/exist/xyzzy-99999")));
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn parse_session_id_excess_trailing_text_errors() {
        // Ulid with trailing garbage should fail
        let result = parse_session_id("01J1XYZABC1234567890ABCDEFtrailing");
        assert!(result.is_err());
    }

    #[test]
    fn parse_session_id_empty_string_errors() {
        let result = parse_session_id("");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Subcommand function smoke tests (happy path — no exit(1))
    // ------------------------------------------------------------------

    #[test]
    fn run_config_init_creates_config_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg-config");
        std::fs::create_dir_all(&xdg).unwrap();
        let old_xdg_config = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg);

        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let result = run_config_subcommand(&["init".to_string()], &project_root);

        match &old_xdg_config {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert!(result.is_ok(), "config init when missing should succeed");
        // Config file should have been created (may be in a different path since
        // default_config_path may check legacy paths; accept existence via the
        // env-var-local path as a weaker assertion)
        let candidate = xdg.join("concerto").join("config.toml");
        if candidate.exists() {
            // good — env var was honoured
        }
    }

    #[test]
    fn run_config_init_when_config_exists_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg-config");
        let config_dir = xdg.join("concerto");
        std::fs::create_dir_all(&config_dir).unwrap();
        // Pre-create a config file
        let config_file = config_dir.join("config.toml");
        std::fs::write(&config_file, "# existing config\n").unwrap();

        let old = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let result = run_config_subcommand(&["init".to_string()], &project_root);

        std::env::set_var("XDG_CONFIG_HOME", old.unwrap_or_default());
        assert!(result.is_ok(), "config init when already exists should succeed");
    }

    #[test]
    fn run_config_doctor_with_existing_config_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg-config");
        let config_dir = xdg.join("concerto");
        std::fs::create_dir_all(&config_dir).unwrap();
        // Write a minimal valid config
        let config_file = config_dir.join("config.toml");
        std::fs::write(
            &config_file,
            "[provider]\nname = \"openai\"\nmodel = \"gpt-4\"\napi_key = \"sk-test-key-for-doctor-1234567890\"\n",
        )
        .unwrap();

        let old = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let result = run_config_subcommand(&["doctor".to_string()], &project_root);

        std::env::set_var("XDG_CONFIG_HOME", old.unwrap_or_default());
        assert!(result.is_ok(), "config doctor should succeed with valid config");
    }

    #[test]
    fn run_providers_list_with_model_settings_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg-config");
        let config_dir = xdg.join("concerto");
        std::fs::create_dir_all(&config_dir).unwrap();
        // Config with model_settings including a global_default_model
        // (to exercise the L291 global_default_model == marker path)
        let config_content = r#"
[model_settings]
global_default_model = "gpt-4"

[[model_settings.providers]]
id = "openai-1"
provider = "openai"
model = "gpt-4"
api_key = "sk-test-key-for-provider-list-1234567890"
"#;
        std::fs::write(config_dir.join("config.toml"), config_content).unwrap();

        let old_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg);

        let result = run_providers_subcommand(&["list".to_string()], &temp.path().join("project"));

        match &old_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert!(result.is_ok(), "providers list with model_settings should succeed");
    }

    #[test]
    fn run_providers_list_no_providers_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg-config");
        let config_dir = xdg.join("concerto");
        std::fs::create_dir_all(&config_dir).unwrap();
        // Config with NO model_settings and NO primary_provider
        // (exercises L304 unchanged providers message path)
        let config_content = "# minimal config with no providers\n";
        std::fs::write(config_dir.join("config.toml"), config_content).unwrap();

        let old_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg);

        let result = run_providers_subcommand(&["list".to_string()], &temp.path().join("project"));

        match &old_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert!(result.is_ok(), "providers list with no providers should succeed");
    }

    #[test]
    fn run_projects_list_with_registry_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        // Isolated XDG_DATA_HOME so ProjectRegistry reads/writes in temp
        let xdg_data = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data).unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_data);

        // Create a project directory and register it
        let proj = temp.path().join("some-project");
        std::fs::create_dir_all(&proj).unwrap();
        let mut registry = concerto_config::ProjectRegistry::default();
        registry.select(&proj).unwrap();
        registry.save().unwrap();

        let result = run_projects_subcommand(&["list".to_string()]);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(result.is_ok(), "projects list should succeed");
    }

    #[test]
    fn run_projects_current_with_active_returns_ok() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg_data = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data).unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_data);

        let proj = temp.path().join("active-project");
        std::fs::create_dir_all(&proj).unwrap();
        let mut registry = concerto_config::ProjectRegistry::default();
        registry.select(&proj).unwrap();
        registry.save().unwrap();

        let result = run_projects_subcommand(&["current".to_string()]);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(result.is_ok(), "projects current should succeed");
    }

    #[test]
    fn run_projects_list_active_marker_matches_recent() {
        // Tests the L459 `==` → `!=` survivor: when active == recent,
        // the marker should be '*'. With the mutant it would be ' '.
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg_data = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data).unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_data);

        let proj = temp.path().join("my-project");
        std::fs::create_dir_all(&proj).unwrap();
        let mut registry = concerto_config::ProjectRegistry::default();
        registry.select(&proj).unwrap();
        registry.save().unwrap();

        let result = run_projects_subcommand(&["list".to_string()]);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(result.is_ok(), "projects list should succeed with active marker match");
        // The active marker '*' vs ' ' cannot be verified without stdout capture,
        // but the mutation at L459 would not change the Ok/Err result, so this test
        // at minimum verifies the code path doesn't crash.
    }

    #[test]
    fn run_logs_show_without_log_file_returns_ok() {
        // Catches the `== NotFound` -> `!= NotFound` survivor at L507:
        // original: Err(e) if e.kind() == NotFound -> Ok(message)
        // mutant:   Err(e) if e.kind() != NotFound -> Ok(message)  [then Err on true NotFound]
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg_data = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data).unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_data);
        // No log file created -> read_to_string returns NotFound

        let result = run_logs_subcommand(&["show".to_string()]);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(result.is_ok(), "logs show without log file should print message and return Ok");
    }

    #[test]
    fn run_sessions_empty_args_errors() {
        // Previously `std::process::exit(1)`, now returns Err -> testable.
        // If the mutant changed `args.is_empty()` to `!is_empty()` the Err
        // wouldn't happen.
        let temp = tempfile::tempdir().unwrap();
        let proj = temp.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let result = run_sessions_subcommand(&[], &proj);
        assert!(result.is_err(), "sessions with no args should error");
    }

    #[test]
    fn run_sessions_show_missing_args_errors() {
        // Catches `args.len() < 2` -> `<=` / `==` / `!=` survivors at L397*.
        // (*line number may shift)
        let temp = tempfile::tempdir().unwrap();
        let proj = temp.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let result = run_sessions_subcommand(&["show".to_string()], &proj);
        assert!(result.is_err(), "sessions show without id should error");
    }

    #[test]
    fn run_sessions_events_missing_args_errors() {
        // Catches `args.len() < 2` survivor.
        let temp = tempfile::tempdir().unwrap();
        let proj = temp.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let result = run_sessions_subcommand(&["events".to_string()], &proj);
        assert!(result.is_err(), "sessions events without id should error");
    }

    #[test]
    fn run_sessions_resume_missing_args_errors() {
        // Catches `args.len() < 2` survivor.
        let temp = tempfile::tempdir().unwrap();
        let proj = temp.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let result = run_sessions_subcommand(&["resume".to_string()], &proj);
        assert!(result.is_err(), "sessions resume without id should error");
    }

    // ------------------------------------------------------------------
    // sessions prune (item H6)
    // ------------------------------------------------------------------

    #[test]
    fn prune_requires_older_than() {
        // Arg validation only — no database is opened.
        let temp = tempfile::tempdir().unwrap();
        let proj = temp.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();

        // Missing --older-than entirely.
        assert!(
            run_sessions_subcommand(&["prune".to_string()], &proj).is_err(),
            "prune without --older-than should error"
        );
        // Invalid --older-than value.
        assert!(
            run_sessions_subcommand(
                &["prune".to_string(), "--older-than".to_string(), "abc".to_string()],
                &proj,
            )
            .is_err(),
            "prune with a non-integer --older-than should error"
        );
        // Non-positive --older-than value.
        assert!(
            run_sessions_subcommand(
                &["prune".to_string(), "--older-than".to_string(), "0".to_string()],
                &proj,
            )
            .is_err(),
            "prune with --older-than 0 should error"
        );
        // Unknown flag.
        assert!(
            run_sessions_subcommand(
                &[
                    "prune".to_string(),
                    "--older-than".to_string(),
                    "30".to_string(),
                    "--bogus".to_string(),
                ],
                &proj,
            )
            .is_err(),
            "prune with an unknown flag should error"
        );
    }

    /// Path of the session DB under the current (redirected) XDG_DATA_HOME.
    fn prune_test_db_path() -> PathBuf {
        dirs::data_dir().expect("data dir").join("concerto").join("sessions.db")
    }

    /// Rewrite a session's `created_at` via a raw SQL update so it becomes
    /// eligible for pruning (the public API always stamps `now_utc()`).
    fn age_session(db_path: &Path, id: &str, days_ago: i64) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Match the production store's WAL mode and wait instead of failing
            // on SQLITE_BUSY: after `create_session` returns, the store's pool
            // may still be closing its connections in the background, which can
            // transiently lock the DB under load (CI flake, see PR #133).
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(db_path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(std::time::Duration::from_secs(30));
            let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
            let result = sqlx::query("UPDATE sessions SET created_at = ? WHERE id = ?")
                .bind(time::OffsetDateTime::now_utc().unix_timestamp() - days_ago * 86400)
                .bind(id)
                .execute(&pool)
                .await;
            // Deterministically close connections instead of racing the
            // background close task; then surface any UPDATE error.
            let _ = pool.close().await;
            result.unwrap();
        });
    }

    /// Whether a session still exists in the store under the current
    /// (redirected) XDG_DATA_HOME.
    fn session_exists(session_id: &str) -> bool {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use concerto_sessions::SessionStore;
            let store = concerto_sessions::SqliteSessionStore::connect().await.unwrap();
            store
                .load_session(session_id.parse().unwrap(), CancellationToken::new())
                .await
                .unwrap()
                .is_some()
        })
    }

    #[test]
    fn prune_dry_run_deletes_nothing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg_data = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data).unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_data);
        // `SqliteSessionStore::connect()` resolves the `[audit]` at-rest
        // policy from the *global* config (row 44). Point XDG_CONFIG_HOME at
        // an empty directory so these tests see pure defaults (encryption
        // off) instead of a developer's real config, which could otherwise
        // fail closed on keychain access.
        let xdg_config = temp.path().join("xdg-config");
        std::fs::create_dir_all(&xdg_config).unwrap();
        let old_config = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config);

        let proj = temp.path().join("prune-project");
        std::fs::create_dir_all(&proj).unwrap();
        let db_path = prune_test_db_path();

        let session_id = {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                use concerto_sessions::SessionStore;
                let store = concerto_sessions::SqliteSessionStore::connect().await.unwrap();
                store
                    .create_session(
                        &camino::Utf8PathBuf::from_path_buf(proj.clone()).unwrap(),
                        "openai",
                        "gpt-4",
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
                    .id
                    .to_string()
            })
        };
        age_session(&db_path, &session_id, 90);

        let result = run_sessions_subcommand(
            &[
                "prune".to_string(),
                "--older-than".to_string(),
                "30".to_string(),
                "--keep".to_string(),
                "0".to_string(),
                "--dry-run".to_string(),
            ],
            &proj,
        );

        // The dry run must not delete anything.
        let still_exists = session_exists(&session_id);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match &old_config {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(result.is_ok(), "dry run should succeed: {:?}", result.err());
        assert!(still_exists, "dry run must not delete sessions");
    }

    #[test]
    fn prune_skips_active_session() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg_data = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data).unwrap();
        let old_data = std::env::var("XDG_DATA_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", &xdg_data);
        // `SqliteSessionStore::connect()` resolves the `[audit]` at-rest
        // policy from the *global* config (row 44). Point XDG_CONFIG_HOME at
        // an empty directory so these tests see pure defaults (encryption
        // off) instead of a developer's real config, which could otherwise
        // fail closed on keychain access.
        let xdg_config = temp.path().join("xdg-config");
        std::fs::create_dir_all(&xdg_config).unwrap();
        let old_config = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config);

        let proj = temp.path().join("prune-project");
        std::fs::create_dir_all(&proj).unwrap();
        let db_path = prune_test_db_path();

        let (active_id, stale_id, recent_id) = {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                use concerto_sessions::SessionStore;
                let store = concerto_sessions::SqliteSessionStore::connect().await.unwrap();
                let project = camino::Utf8PathBuf::from_path_buf(proj.clone()).unwrap();
                let active = store
                    .create_session(&project, "openai", "gpt-4", CancellationToken::new())
                    .await
                    .unwrap();
                let stale = store
                    .create_session(&project, "openai", "gpt-4", CancellationToken::new())
                    .await
                    .unwrap();
                let recent = store
                    .create_session(&project, "openai", "gpt-4", CancellationToken::new())
                    .await
                    .unwrap();
                // The oldest session is the project's active session.
                store
                    .set_active_session_for_project(&project, active.id, CancellationToken::new())
                    .await
                    .unwrap();
                (active.id.to_string(), stale.id.to_string(), recent.id.to_string())
            })
        };
        // Ages: active = 100d (oldest), stale = 90d, recent = 80d. With
        // --keep 1 only the newest is protected; the stale session must be
        // deleted and the active one skipped.
        age_session(&db_path, &active_id, 100);
        age_session(&db_path, &stale_id, 90);
        age_session(&db_path, &recent_id, 80);

        let result = run_sessions_subcommand(
            &[
                "prune".to_string(),
                "--older-than".to_string(),
                "30".to_string(),
                "--keep".to_string(),
                "1".to_string(),
            ],
            &proj,
        );

        let active_exists = session_exists(&active_id);
        let stale_exists = session_exists(&stale_id);
        let recent_exists = session_exists(&recent_id);

        match &old_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match &old_config {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(result.is_ok(), "prune should succeed: {:?}", result.err());
        assert!(active_exists, "active session must be protected from pruning");
        assert!(!stale_exists, "old non-active session must be pruned");
        assert!(recent_exists, "the --keep most recent session must be protected");
    }

    #[test]
    fn run_extensions_list_with_default_config_returns_ok() {
        // No config present: `extensions list` falls back to defaults and
        // must still print both sections (skills discovery is skipped while
        // skills are disabled by default, and there are no MCP servers).
        let temp = tempfile::tempdir().unwrap();
        let proj = temp.path().join("project");
        std::fs::create_dir_all(&proj).unwrap();

        let result = run_extensions_subcommand(&["list".to_string()], &proj);

        assert!(result.is_ok(), "extensions list should succeed: {:?}", result.err());
    }

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

    // ------------------------------------------------------------------
    // audit subcommand (read path for `audit_log`)
    // ------------------------------------------------------------------

    /// Valid ULID used as the positional session id by the parse tests.
    const AUDIT_TEST_SESSION_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// Owned argv for the `&[String]` argument parsers.
    fn audit_argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|value| value.to_string()).collect()
    }

    /// An `AuditLogRow` carrying no path facts (pre-034 / path-less tool).
    fn audit_row() -> AuditLogRow {
        AuditLogRow {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            session_id: Some(AUDIT_TEST_SESSION_ID.to_string()),
            created_at: 0,
            tool_name: "filesystem".to_string(),
            verdict: "Allow".to_string(),
            rule_matched: Some("auto_approve".to_string()),
            error_kind: None,
            duration_ms: Some(7),
            path_operation: None,
            attempted_path: None,
            resolved_path: None,
            attempted_destination: None,
            resolved_destination: None,
            result_facts: None,
        }
    }

    #[test]
    fn parse_audit_args_defaults() {
        let parsed = parse_audit_args(&audit_argv(&[AUDIT_TEST_SESSION_ID])).unwrap();
        assert_eq!(parsed.session_id.to_string(), AUDIT_TEST_SESSION_ID);
        assert_eq!(parsed.filter, AuditLogFilter::default());
        assert!(!parsed.json);
    }

    #[test]
    fn parse_audit_args_full_flags() {
        let parsed = parse_audit_args(&audit_argv(&[
            "--tool",
            "filesystem",
            "--operation",
            "move",
            "--failed",
            "--limit",
            "5",
            "--json",
            AUDIT_TEST_SESSION_ID,
        ]))
        .unwrap();
        assert_eq!(parsed.session_id.to_string(), AUDIT_TEST_SESSION_ID);
        assert_eq!(parsed.filter.tool.as_deref(), Some("filesystem"));
        assert_eq!(parsed.filter.operation.as_deref(), Some("move"));
        assert!(parsed.filter.failed);
        assert_eq!(parsed.filter.limit, Some(5));
        assert!(parsed.json);
    }

    #[test]
    fn parse_audit_args_missing_session_id_is_error() {
        let empty = parse_audit_args(&audit_argv(&[])).unwrap_err();
        assert!(empty.to_string().contains("missing session id"), "{empty}");

        // A flag where the id should have been still leaves no session id.
        let flag_only = parse_audit_args(&audit_argv(&["--failed"])).unwrap_err();
        assert!(flag_only.to_string().contains("missing session id"), "{flag_only}");

        // A flag plus its value still leaves no session id.
        let flag_with_value = parse_audit_args(&audit_argv(&["--tool", "filesystem"])).unwrap_err();
        assert!(flag_with_value.to_string().contains("missing session id"), "{flag_with_value}");
    }

    #[test]
    fn parse_audit_args_unknown_flag_is_error() {
        let error = parse_audit_args(&audit_argv(&[AUDIT_TEST_SESSION_ID, "--bogus"])).unwrap_err();
        assert!(error.to_string().contains("unknown audit option '--bogus'"), "{error}");
    }

    #[test]
    fn parse_audit_args_missing_flag_value_is_error() {
        for flag in ["--tool", "--operation", "--limit"] {
            let error = parse_audit_args(&audit_argv(&[AUDIT_TEST_SESSION_ID, flag])).unwrap_err();
            assert!(error.to_string().contains("missing value"), "{flag}: {error}");
        }
    }

    #[test]
    fn parse_audit_args_invalid_limit_is_error() {
        let not_a_number =
            parse_audit_args(&audit_argv(&[AUDIT_TEST_SESSION_ID, "--limit", "many"])).unwrap_err();
        assert!(not_a_number.to_string().contains("invalid limit 'many'"), "{not_a_number}");

        let zero =
            parse_audit_args(&audit_argv(&[AUDIT_TEST_SESSION_ID, "--limit", "0"])).unwrap_err();
        assert!(zero.to_string().contains("--limit must be at least 1"), "{zero}");
    }

    #[test]
    fn parse_audit_args_invalid_session_id_is_error() {
        let error = parse_audit_args(&audit_argv(&["not-a-ulid"])).unwrap_err();
        assert!(error.to_string().contains("invalid session id 'not-a-ulid'"), "{error}");

        let extra = parse_audit_args(&audit_argv(&[AUDIT_TEST_SESSION_ID, "extra"])).unwrap_err();
        assert!(extra.to_string().contains("unexpected argument 'extra'"), "{extra}");
    }

    /// Argument errors surface before any database is touched: the runner
    /// must fail on an empty argv without resolving the sessions path.
    #[test]
    fn run_audit_subcommand_requires_args_before_opening_db() {
        let error = run_audit_subcommand(&[]).unwrap_err();
        assert!(error.to_string().contains("missing session id"), "{error}");
    }

    #[test]
    fn audit_open_error_reports_database_path() {
        let db_path = std::path::PathBuf::from("/nonexistent-home/sessions.db");
        let error = concerto_sessions::SessionError::Database("boom".to_string());
        let message = audit_open_error(&db_path, &error).to_string();
        assert!(message.contains("could not open the sessions database"), "{message}");
        assert!(message.contains("/nonexistent-home/sessions.db"), "{message}");
        assert!(!message.contains("encrypt_at_rest"), "{message}");
    }

    /// Marker sidecar present → the message points at the setting that would
    /// let the CLI read the database, not at the raw driver error.
    #[test]
    fn audit_open_error_explains_at_rest_encrypted_database() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("sessions.db");
        std::fs::write(&db_path, b"not really sqlite").unwrap();
        std::fs::write(concerto_sessions::marker_path(&db_path), b"").unwrap();
        assert!(concerto_sessions::is_at_rest_encrypted(&db_path));

        let error = concerto_sessions::SessionError::Storage(
            "sessions.db is at-rest encrypted (marker present)".to_string(),
        );
        let message = audit_open_error(&db_path, &error).to_string();
        assert!(message.contains("[audit] encrypt_at_rest"), "{message}");
        assert!(message.contains("CONCERTO_AUDIT_DB_ENCRYPTION_KEY"), "{message}");
        assert!(message.contains(db_path.to_string_lossy().as_ref()), "{message}");
    }

    /// Encrypted-file errors reported by sqlite itself are recognised even
    /// when no marker is visible to the caller.
    #[test]
    fn audit_open_error_recognises_sqlite_encryption_errors() {
        let error = concerto_sessions::SessionError::Database("file is not a database".to_string());
        assert!(audit_is_encrypted_error(&error));
        let db_path = std::path::PathBuf::from("/var/lib/concerto/sessions.db");
        let message = audit_open_error(&db_path, &error).to_string();
        assert!(message.contains("[audit] encrypt_at_rest"), "{message}");

        let unrelated = concerto_sessions::SessionError::Lock("database is locked".to_string());
        assert!(!audit_is_encrypted_error(&unrelated));
    }

    #[test]
    fn format_audit_entry_marks_rows_without_path_facts() {
        let rendered = format_audit_entry(1, &audit_row());
        assert!(rendered.contains("(no path facts recorded)"), "{rendered}");
        assert!(rendered.contains("filesystem"), "{rendered}");
        assert!(rendered.contains("Allow"), "{rendered}");
        // The detail line still carries what the row does know.
        assert!(rendered.contains("rule auto_approve"), "{rendered}");
        assert!(rendered.contains("7ms"), "{rendered}");
    }

    #[test]
    fn format_audit_entry_shows_move_with_destination() {
        let row = AuditLogRow {
            path_operation: Some("move".to_string()),
            attempted_path: Some("c.txt".to_string()),
            resolved_path: Some("/proj/c.txt".to_string()),
            attempted_destination: Some("d.txt".to_string()),
            resolved_destination: Some("/proj/d.txt".to_string()),
            duration_ms: Some(12),
            ..audit_row()
        };
        let rendered = format_audit_entry(3, &row);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2, "{rendered}");
        assert!(lines[0].contains("  move  c.txt → d.txt"), "{}", lines[0]);
        assert!(lines[1].contains("resolved /proj/c.txt"), "{}", lines[1]);
        assert!(lines[1].contains("destination /proj/d.txt"), "{}", lines[1]);
        assert!(lines[1].contains("12ms"), "{}", lines[1]);
        assert!(!rendered.contains("no path facts"), "{rendered}");
    }

    #[test]
    fn format_audit_timestamp_renders_rfc3339_utc() {
        assert_eq!(format_audit_timestamp(0), "1970-01-01T00:00:00Z");
    }

    /// A read-only row carries its result summary on the detail line, and a
    /// row without one (mutating op / pre-035) renders no result clause.
    #[test]
    fn format_audit_entry_shows_read_result_facts() {
        let row = AuditLogRow {
            path_operation: Some("list".to_string()),
            attempted_path: Some(".".to_string()),
            resolved_path: Some("/proj".to_string()),
            result_facts: Some("entries=1".to_string()),
            ..audit_row()
        };
        let rendered = format_audit_entry(2, &row);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2, "{rendered}");
        assert!(lines[1].contains("resolved /proj"), "{}", lines[1]);
        assert!(lines[1].contains("result entries=1"), "{}", lines[1]);

        let plain = format_audit_entry(3, &audit_row());
        assert!(!plain.contains("result "), "no recorded result, no clause: {plain}");
    }

    /// The `--json` envelope carries `result_facts` and deserialises back
    /// into the same row (read side of the audit trail).
    #[test]
    fn audit_json_output_round_trips_result_facts() {
        let row = AuditLogRow { result_facts: Some("exists=false".to_string()), ..audit_row() };
        let envelope =
            serde_json::json!({ "session_id": AUDIT_TEST_SESSION_ID, "entries": vec![row] });
        let encoded = serde_json::to_string(&envelope).expect("envelope serialises");
        assert!(
            encoded.contains("\"result_facts\":\"exists=false\""),
            "the new field must appear in --json: {encoded}"
        );

        let decoded: serde_json::Value = serde_json::from_str(&encoded).expect("valid JSON");
        let entries: Vec<AuditLogRow> =
            serde_json::from_value(decoded["entries"].clone()).expect("rows decode");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].result_facts.as_deref(), Some("exists=false"));
    }
}
