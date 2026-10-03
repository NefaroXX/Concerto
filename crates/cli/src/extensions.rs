//! CLI counterparts of the desktop extension and shell management actions.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use concerto_config::settings::{redact_settings, SettingsScope};

use crate::settings;

fn exact(args: &[String], count: usize, usage: &str) -> anyhow::Result<()> {
    if args.len() != count {
        bail!("usage: concerto {usage}");
    }
    Ok(())
}

fn output(value: &impl serde::Serialize) -> anyhow::Result<()> {
    let mut value = serde_json::to_value(value)?;
    redact_settings(&mut value);
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn toml_value(value: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(toml::Value::try_from(value)?.to_string())
}

fn read_typed_file<T: serde::de::DeserializeOwned + serde::Serialize>(
    path: &str,
) -> anyhow::Result<T> {
    let raw = std::fs::read_to_string(path)?;
    let typed: T = toml::from_str(&raw)?;
    let supplied = serde_json::to_value(toml::from_str::<toml::Value>(&raw)?)?;
    concerto_config::settings::reject_unknown_keys(
        &supplied,
        &serde_json::to_value(&typed)?,
        "record",
    )?;
    Ok(typed)
}

pub fn run(args: &[String], project: &Path) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("skills") => run_skills(&args[1..], project),
        Some("mcp") => run_mcp(&args[1..], project),
        _ => bail!("usage: concerto extensions <list|skills ACTION|mcp ACTION>\nUse config set for master enable flags, search paths, auto-load, and project context."),
    }
}

fn run_skills(args: &[String], project: &Path) -> anyhow::Result<()> {
    let usage = "extensions skills <list|show ID|create MANIFEST PARENT|edit ID MANIFEST|remove ID --yes|enable ID|disable ID>";
    let editor = settings::editor(project, SettingsScope::Global)?;
    let config = concerto_config::load_global_config(Some(&editor.global_path))?;
    let mut skills = config.skills.unwrap_or_default();
    let manager =
        concerto_skills::SkillManager::new(skills.search_paths.iter().map(PathBuf::from).collect());
    let found = manager.discover()?;
    match args.first().map(String::as_str) {
        Some("list") => {
            exact(args, 1, "extensions skills list")?;
            let report = manager.discover_with_report()?;
            output(&report.descriptors)?;
            for warning in report.warnings {
                eprintln!("{warning:?}");
            }
        }
        Some("show") => {
            exact(args, 2, "extensions skills show ID")?;
            output(&found.iter().find(|skill| skill.id == args[1]).context("unknown skill id")?)?;
        }
        Some("create") => {
            exact(args, 3, "extensions skills create MANIFEST PARENT")?;
            let manifest: concerto_skills::SkillManifest = read_typed_file(&args[1])?;
            let parent = PathBuf::from(&args[2]);
            let expanded = concerto_skills::expanded_search_path(&parent)
                .context("cannot expand skill parent")?;
            let add_parent = !skills.search_paths.contains(&args[2]);
            if add_parent {
                skills.search_paths.push(args[2].clone());
                editor.edit(
                    "skills.search_paths",
                    Some(&toml_value(&skills.search_paths)?),
                    true,
                )?;
            }
            let directory = manager.create_pack(&expanded, &manifest)?;
            if add_parent {
                editor.edit(
                    "skills.search_paths",
                    Some(&toml_value(&skills.search_paths)?),
                    false,
                )?;
            }
            println!(
                "Created {}. Use extensions skills enable ID if an allowlist is configured.",
                directory.display()
            );
        }
        Some("edit") => {
            exact(args, 3, "extensions skills edit ID MANIFEST")?;
            let skill =
                found.iter().find(|skill| skill.id == args[1]).context("unknown skill id")?;
            let manifest: concerto_skills::SkillManifest = read_typed_file(&args[2])?;
            manager.update_pack(&skill.pack_dir, &manifest)?;
            println!("Saved skill pack. Applies to the next run.");
        }
        Some("remove") => {
            exact(args, 3, "extensions skills remove ID --yes")?;
            if args[2] != "--yes" {
                bail!("skill removal requires --yes");
            }
            let skill =
                found.iter().find(|skill| skill.id == args[1]).context("unknown skill id")?;
            output(&manager.delete_pack(&skill.pack_dir)?)?;
        }
        Some(action @ ("enable" | "disable")) => {
            exact(args, 2, "extensions skills <enable|disable> ID")?;
            if !found.iter().any(|skill| skill.id == args[1]) {
                bail!("unknown skill id");
            }
            let mut ids = skills
                .enabled_ids
                .unwrap_or_else(|| found.iter().map(|skill| skill.id.clone()).collect());
            if action == "disable" {
                ids.retain(|id| id != &args[1]);
            } else if !ids.contains(&args[1]) {
                ids.push(args[1].clone());
            }
            editor.edit("skills.enabled_ids", Some(&toml_value(&ids)?), false)?;
            println!("Saved skill allowlist. Applies to the next run; the skills master switch still governs loading.");
        }
        _ => bail!("usage: concerto {usage}"),
    }
    Ok(())
}

fn run_mcp(args: &[String], project: &Path) -> anyhow::Result<()> {
    let usage = "extensions mcp <list|show ID|add SERVER_TOML|set ID KEY TOML_VALUE|remove ID --yes|enable ID|disable ID|probe ID>";
    let editor = settings::editor(project, SettingsScope::Global)?;
    let config = concerto_config::load_global_config(Some(&editor.global_path))?;
    let mut mcp = config.mcp.unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("list") => {
            exact(args, 1, "extensions mcp list")?;
            output(&mcp)?;
            return Ok(());
        }
        Some("show") => {
            exact(args, 2, "extensions mcp show ID")?;
            output(
                &mcp.servers
                    .iter()
                    .find(|server| server.id == args[1])
                    .context("unknown MCP server id")?,
            )?;
            return Ok(());
        }
        Some("add") => {
            exact(args, 2, "extensions mcp add SERVER_TOML")?;
            let server: concerto_config::McpServerConfig = read_typed_file(&args[1])?;
            if mcp.servers.iter().any(|existing| existing.id == server.id) {
                bail!("MCP server id already exists; use set to edit it");
            }
            mcp.servers.push(server);
        }
        Some("set") => {
            exact(args, 4, "extensions mcp set ID KEY TOML_VALUE")?;
            if args[2] == "id" {
                bail!("MCP server identity is immutable");
            }
            let index = mcp
                .servers
                .iter()
                .position(|server| server.id == args[1])
                .context("unknown MCP server id")?;
            editor.edit(&format!("mcp.servers.{index}.{}", args[2]), Some(&args[3]), false)?;
            println!("Saved MCP server setting. Applies to the next run.");
            return Ok(());
        }
        Some("remove") => {
            exact(args, 3, "extensions mcp remove ID --yes")?;
            if args[2] != "--yes" {
                bail!("MCP server removal requires --yes");
            }
            if !mcp.servers.iter().any(|server| server.id == args[1]) {
                bail!("unknown MCP server id");
            }
            mcp.servers.retain(|server| server.id != args[1]);
        }
        Some(action @ ("enable" | "disable")) => {
            exact(args, 2, "extensions mcp <enable|disable> ID")?;
            mcp.servers
                .iter_mut()
                .find(|server| server.id == args[1])
                .context("unknown MCP server id")?
                .enabled = action == "enable";
        }
        Some("probe") => {
            exact(args, 2, "extensions mcp probe ID")?;
            // A probe is an explicit one-shot request, including for disabled
            // entries, matching Desktop. Never persist these temporary flags.
            let effective = concerto_config::load_config(Some(&editor.global_path), Some(project))?
                .mcp
                .unwrap_or_default();
            let mut server = effective
                .servers
                .iter()
                .find(|server| server.id == args[1])
                .context("unknown MCP server id")?
                .clone();
            server.enabled = true;
            let timeout =
                std::time::Duration::from_secs(server.timeout_secs.unwrap_or(60).clamp(1, 120));
            let manager = concerto_mcp::McpManager::new(
                concerto_config::McpConfig { enabled: true, servers: vec![server] },
                concerto_core::event::EventBus::new(64),
            );
            let rt = tokio::runtime::Runtime::new()?;
            return rt.block_on(async {
                let mut registry = concerto_core::types::ToolRegistry::default();
                let result: anyhow::Result<usize> = tokio::select! {
                    result = tokio::time::timeout(timeout, manager.start_server(&args[1], &mut registry)) => {
                        match result {
                            Ok(result) => result.map_err(Into::into),
                            Err(_) => Err(anyhow::anyhow!("MCP probe timed out")),
                        }
                    }
                    signal = tokio::signal::ctrl_c() => {
                        match signal {
                            Ok(()) => Err(anyhow::anyhow!("MCP probe cancelled")),
                            Err(error) => Err(error.into()),
                        }
                    }
                };
                let tools = manager.tools_for(&args[1]);
                manager.stop_all(&mut registry).await;
                result?;
                output(&tools)
            });
        }
        _ => bail!("usage: concerto {usage}"),
    }
    editor.edit("mcp.servers", Some(&toml_value(&mcp.servers)?), false)?;
    println!(
        "Saved MCP servers. Applies to the next run; the MCP master switch still governs loading."
    );
    Ok(())
}

pub fn run_plugin(args: &[String]) -> anyhow::Result<()> {
    let directory = concerto_plugins::discovery::plugins_dir();
    match args.first().map(String::as_str) {
        Some("installed") => {
            exact(args, 1, "plugin installed")?;
            let discovery = concerto_plugins::discovery::PluginDiscovery::new(concerto_plugins::discovery::DiscoveryConfig { search_paths: vec![directory], bundled_path: None });
            for plugin in discovery.discover()? { println!("{}", plugin.wasm_path.display()); }
        }
        Some("install") => {
            if !(args.len() == 2 || args.len() == 3 && args[2] == "--replace") { bail!("usage: concerto plugin install FILE [--replace]"); }
            let host = Arc::new(concerto_plugins::host::PluginHost::new()?);
            let installer = concerto_plugins::installer::PluginInstaller::new(host);
            let validated = tokio::runtime::Runtime::new()?.block_on(installer.validate(Path::new(&args[1])))?;
            if directory.join(format!("{}.wasm", validated.manifest.id)).exists() && args.len() != 3 { bail!("plugin already exists; use --replace to replace it"); }
            let installed = installer.write(&validated, &directory)?;
            if installed.replaced { concerto_plugins::capability::CapabilityManager::open(&directory)?.revoke_plugin(&validated.manifest.id)?; }
            println!("Installed {}. Capability consent is still required at runtime.", installed.wasm_path.display());
        }
        Some("remove") => {
            exact(args, 3, "plugin remove ID --yes")?;
            if args[2] != "--yes" { bail!("plugin removal requires --yes"); }
            if args[1].is_empty() || args[1] == "." || args[1] == ".." || !args[1].chars().all(|ch| ch.is_ascii_alphanumeric() || "._-".contains(ch)) { bail!("invalid plugin id"); }
            concerto_plugins::installer::delete_plugin_file(&directory.join(format!("{}.wasm", args[1])))?;
            concerto_plugins::capability::CapabilityManager::open(&directory)?.revoke_plugin(&args[1])?;
            println!("Removed plugin file and persisted capability grants. Restart running Desktop instances to reload installed plugins.");
        }
        _ => bail!("usage: concerto plugin <list|installed|install FILE [--replace]|remove ID --yes|revoke ID>"),
    }
    Ok(())
}

pub fn run_shell(args: &[String], project: &Path) -> anyhow::Result<()> {
    let usage = "shell <list|test ID|select ID|managed <install SOURCE|remove --yes|verify|export FILE|import FILE>>";
    let editor = settings::editor(project, SettingsScope::Global)?;
    let config = concerto_config::load_config(Some(&editor.global_path), Some(project))?;
    let shells = config.shell_settings.unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("list") => {
            exact(args, 1, "shell list")?;
            output(&shells)?;
        }
        Some("test") => {
            exact(args, 2, "shell test ID")?;
            let profile = shells
                .profiles
                .iter()
                .find(|profile| profile.id == args[1])
                .context("unknown shell profile")?;
            output(&profile.availability())?;
        }
        Some("select") => {
            exact(args, 2, "shell select ID")?;
            // Materialize detected profiles only when no global settings exist.
            let global = concerto_config::load_global_config(Some(&editor.global_path))?;
            let global_shells = global.shell_settings.clone().unwrap_or_default();
            if !global_shells.profiles.iter().any(|profile| profile.id == args[1]) {
                bail!("unknown global shell profile; edit project selection with config set --project-scope");
            }
            if global.shell_settings.is_none() {
                editor.edit("shell_settings", Some(&toml_value(&global_shells)?), false)?;
            }
            editor.edit("shell_settings.selected_profile", Some(&toml_value(&args[1])?), false)?;
            println!("Saved shell selection. Applies to the next run; project overrides may take precedence.");
        }
        Some("managed") => {
            let manager = concerto_config::ManagedRuntimeManager::for_data_dir()?;
            match args.get(1).map(String::as_str) {
                Some("install") => {
                    exact(args, 3, "shell managed install SOURCE")?;
                    output(&manager.install_from(Path::new(&args[2]))?)?;
                }
                Some("remove") => {
                    exact(args, 3, "shell managed remove --yes")?;
                    if args[2] != "--yes" {
                        bail!("managed runtime removal requires --yes");
                    }
                    manager.remove()?;
                    println!("Managed runtime removed.");
                }
                Some("verify") => {
                    exact(args, 2, "shell managed verify")?;
                    let manifest = read_runtime_manifest(&manager)?;
                    let report = manager.verify(&manifest)?;
                    println!("{report:#?}");
                    if !report.runtime_ok {
                        bail!("managed runtime integrity verification failed");
                    }
                }
                Some("export") => {
                    exact(args, 3, "shell managed export FILE")?;
                    let manifest = read_runtime_manifest(&manager)?;
                    std::fs::write(
                        &args[2],
                        concerto_config::ManagedRuntimeManager::export_manifest(&manifest)?,
                    )?;
                    println!("Exported managed runtime manifest.");
                }
                Some("import") => {
                    exact(args, 3, "shell managed import FILE")?;
                    let manifest = concerto_config::ManagedRuntimeManager::import_manifest(
                        &std::fs::read_to_string(&args[2])?,
                    )?;
                    let report = manager.verify(&manifest)?;
                    println!("{report:#?}");
                    if !report.runtime_ok {
                        bail!("manifest integrity verification failed; nothing written");
                    }
                    std::fs::create_dir_all(manager.root())?;
                    std::fs::write(
                        manager.manifest_path(),
                        concerto_config::ManagedRuntimeManager::export_manifest(&manifest)?,
                    )?;
                    println!("Imported manifest; runtime binaries are not copied.");
                }
                _ => bail!("usage: concerto {usage}"),
            }
        }
        _ => bail!("usage: concerto {usage}"),
    }
    Ok(())
}

fn read_runtime_manifest(
    manager: &concerto_config::ManagedRuntimeManager,
) -> anyhow::Result<concerto_config::RuntimeManifest> {
    Ok(serde_json::from_str(&std::fs::read_to_string(manager.manifest_path())?)?)
}
