//! Command equivalents for desktop Settings and Studio configuration.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use concerto_config::settings::{redact_settings, value_at, SettingsEditor, SettingsScope};
use concerto_config::{AppConfig, CustomAgentConfig};
use serde_json::Value;

pub const CONFIG_HELP: &str = "config show [--global|--effective|--example] [--json]
config get KEY [--global|--effective|--project-scope]
config keys [PREFIX]
config set KEY TOML_VALUE [--global|--project-scope] [--dry-run]
config set KEY --file FILE [--global|--project-scope] [--dry-run]
config unset KEY [--global|--project-scope] [--dry-run]
config path [--global|--project-scope]
config init | config doctor

Default reads show effective settings; default writes change global settings.
Use numeric path segments for existing array entries (mcp.servers.0.enabled).
--file reads one TOML value, including multiline strings and record arrays.
Environment additions are redacted. Saved changes apply to the next run.
Use agents commands for the canonical roster; credentials use the OS keychain.";

pub fn editor(project: &Path, scope: SettingsScope) -> anyhow::Result<SettingsEditor> {
    Ok(SettingsEditor {
        global_path: concerto_config::default_config_path()
            .context("cannot determine config directory")?,
        project_root: project.to_owned(),
        scope,
    })
}

fn flag_args(args: &[String], allowed: &[&str]) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut literal = false;
    for arg in args {
        if arg == "--" && !literal {
            literal = true;
            continue;
        }
        if !literal && arg.starts_with("--") {
            if !allowed.contains(&arg.as_str()) {
                bail!("unknown option: {arg}");
            }
            if flags.contains(arg) {
                bail!("duplicate option: {arg}");
            }
            flags.push(arg.clone());
        } else {
            positional.push(arg.clone());
        }
    }
    Ok((positional, flags))
}

fn has(flags: &[String], flag: &str) -> bool {
    flags.iter().any(|value| value == flag)
}

fn selected_scope(flags: &[String]) -> anyhow::Result<SettingsScope> {
    if has(flags, "--global") && has(flags, "--project-scope") {
        bail!("--global and --project-scope are mutually exclusive");
    }
    Ok(if has(flags, "--project-scope") { SettingsScope::Project } else { SettingsScope::Global })
}

fn print_value(mut value: Value) -> anyhow::Result<()> {
    redact_settings(&mut value);
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn raw_value(raw: &str) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(toml::from_str::<toml::Value>(raw)?)?)
}

fn typed_value<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    let value = toml::Value::try_from(value)?;
    Ok(value.to_string())
}

fn effective(project: &Path) -> anyhow::Result<AppConfig> {
    Ok(concerto_config::load_config(
        concerto_config::default_config_path().as_ref(),
        Some(project),
    )?)
}

fn global(project: &Path) -> anyhow::Result<AppConfig> {
    Ok(concerto_config::load_global_config(Some(
        &editor(project, SettingsScope::Global)?.global_path,
    ))?)
}

fn exact(args: &[String], count: usize, usage: &str) -> anyhow::Result<()> {
    if args.len() != count {
        bail!("usage: concerto {usage}");
    }
    Ok(())
}

pub fn run_config(args: &[String], project: &Path) -> anyhow::Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("help");
    if command == "help" {
        println!("{CONFIG_HELP}");
        return Ok(());
    }
    // --file has a value, so consume it before processing boolean options.
    let mut input_file = None;
    let mut filtered = Vec::new();
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--file" {
            index += 1;
            let path = args.get(index).context("--file requires a path")?;
            if input_file.replace(PathBuf::from(path)).is_some() {
                bail!("duplicate --file");
            }
        } else {
            filtered.push(args[index].clone());
        }
        index += 1;
    }
    let allowed = match command {
        "show" => vec!["--global", "--effective", "--example", "--json"],
        "get" => vec!["--global", "--effective", "--project-scope"],
        "set" | "unset" => vec!["--global", "--project-scope", "--dry-run"],
        "path" => vec!["--global", "--project-scope"],
        "keys" => vec![],
        _ => bail!("unknown config command '{command}'; run `config help`"),
    };
    let (positional, flags) = flag_args(&filtered, &allowed)?;
    if input_file.is_some() && command != "set" {
        bail!("--file is only supported by config set");
    }
    if ["--global", "--effective", "--example", "--project-scope"]
        .iter()
        .filter(|flag| has(&flags, flag))
        .count()
        > 1
    {
        bail!("choose one settings view/scope");
    }
    let editor = editor(project, selected_scope(&flags)?)?;
    match command {
        "path" => {
            exact(&positional, 0, "config path [--global|--project-scope]")?;
            println!("{}", editor.path().display());
        }
        "show" => {
            exact(&positional, 0, "config show [--global|--effective|--example] [--json]")?;
            let config = if has(&flags, "--example") {
                concerto_config::settings::settings_example()
            } else if has(&flags, "--global") {
                global(project)?
            } else {
                effective(project)?
            };
            let mut value = serde_json::to_value(config)?;
            redact_settings(&mut value);
            if has(&flags, "--json") {
                print_value(value)?;
            } else {
                print!("{}", toml::to_string_pretty(&serde_json::from_value::<AppConfig>(value)?)?);
            }
        }
        "get" => {
            exact(&positional, 1, "config get KEY [--global|--effective|--project-scope]")?;
            let mut value = if has(&flags, "--project-scope") {
                raw_value(
                    &std::fs::read_to_string(editor.path()).context("no saved project settings")?,
                )?
            } else {
                serde_json::to_value(if has(&flags, "--global") {
                    global(project)?
                } else {
                    effective(project)?
                })?
            };
            redact_settings(&mut value);
            print_value(
                value_at(&value, &positional[0]).context("unknown or unset setting path")?.clone(),
            )?;
        }
        "keys" => {
            if positional.len() > 1 {
                bail!("usage: concerto config keys [PREFIX]");
            }
            let example = serde_json::to_value(concerto_config::settings::settings_example())?;
            let mut keys = Vec::new();
            setting_keys(&example, "", &mut keys);
            for key in keys
                .into_iter()
                .filter(|key| positional.first().is_none_or(|prefix| key.starts_with(prefix)))
            {
                println!("{key}");
            }
        }
        "set" | "unset" => {
            exact(
                &positional,
                if command == "set" && input_file.is_none() { 2 } else { 1 },
                "config set KEY TOML_VALUE | config set KEY --file FILE | config unset KEY",
            )?;
            let from_file = input_file.map(std::fs::read_to_string).transpose()?;
            let value = if command == "unset" {
                None
            } else {
                from_file.as_deref().or_else(|| positional.get(1).map(String::as_str))
            };
            let dry_run = has(&flags, "--dry-run");
            let document = editor.edit(&positional[0], value, dry_run)?;
            if dry_run {
                print_value(raw_value(&document)?)?;
            } else {
                println!("Saved {} in {}. Applies to the next run; project/env overrides may take precedence.", positional[0], editor.path().display());
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn setting_keys(value: &Value, path: &str, keys: &mut Vec<String>) {
    if let Some(map) = value.as_object() {
        if map.is_empty() && !path.is_empty() {
            keys.push(path.to_string());
        }
        for (key, value) in map {
            let key = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
            setting_keys(value, &key, keys);
        }
    } else if let Some(array) =
        value.as_array().filter(|array| !array.is_empty() && array[0].is_object())
    {
        keys.push(path.to_string());
        setting_keys(&array[0], &format!("{path}.0"), keys);
    } else {
        keys.push(path.to_string());
    }
}

fn roster(config: &AppConfig) -> Vec<CustomAgentConfig> {
    if config.owns_agent_roster() {
        config.multi_agent.as_ref().map(|multi| multi.custom_agents.clone()).unwrap_or_default()
    } else {
        concerto_config::builtin_agent_seeds()
    }
}

pub fn run_agents(args: &[String], project: &Path) -> anyhow::Result<()> {
    let usage =
        "agents <list|show ID|import FILE|set ID KEY TOML_VALUE|clone ID NEW_ID|remove ID --yes>";
    let command = args.first().map(String::as_str).unwrap_or("help");
    if command == "help" {
        println!("{usage}\nAgent files and settings are global. Capabilities request access; policy still governs tools.");
        return Ok(());
    }
    let config = global(project)?;
    let mut agents = roster(&config);
    let path = editor(project, SettingsScope::Global)?.global_path;
    match command {
        "list" => {
            exact(args, 1, "agents list")?;
            print_value(serde_json::to_value(agents)?)?;
            return Ok(());
        }
        "show" => {
            exact(args, 2, "agents show ID")?;
            print_value(serde_json::to_value(
                agents.iter().find(|agent| agent.id == args[1]).context("unknown agent id")?,
            )?)?;
            return Ok(());
        }
        "import" => {
            exact(args, 2, "agents import FILE")?;
            let raw = std::fs::read_to_string(&args[1])?;
            let agent = concerto_config::AgentFile::from_toml_str(&raw, Path::new(&args[1]))?.agent;
            let mut supplied = raw_value(&raw)?;
            if let Some(map) = supplied.as_object_mut() {
                map.remove("schema_version");
            }
            concerto_config::settings::reject_unknown_keys(
                &supplied,
                &serde_json::to_value(&agent)?,
                "agent",
            )?;
            if let Some(existing) = agents.iter_mut().find(|entry| entry.id == agent.id) {
                *existing = agent;
            } else {
                agents.push(agent);
            }
        }
        "set" => {
            exact(args, 4, "agents set ID KEY TOML_VALUE")?;
            if args[2] == "id" {
                bail!("agent identity is immutable; use agents clone");
            }
            let agent =
                agents.iter_mut().find(|agent| agent.id == args[1]).context("unknown agent id")?;
            let raw = concerto_config::settings::edit_document(
                &toml::to_string_pretty(agent)?,
                &args[2],
                Some(&args[3]),
            )?;
            let edited: CustomAgentConfig = toml::from_str(&raw)?;
            concerto_config::settings::reject_unknown_keys(
                &raw_value(&raw)?,
                &serde_json::to_value(&edited)?,
                "agent",
            )?;
            *agent = edited;
        }
        "clone" => {
            exact(args, 3, "agents clone ID NEW_ID")?;
            if agents.iter().any(|agent| agent.id == args[2]) {
                bail!("agent id already exists");
            }
            let mut agent = agents
                .iter()
                .find(|agent| agent.id == args[1])
                .context("unknown agent id")?
                .clone();
            if agent.model_override.is_none() {
                agent.model_override = config.multi_agent.as_ref().and_then(|m| {
                    m.model_pins
                        .iter()
                        .find(|(id, _)| id.as_str() == args[1])
                        .map(|(_, model)| model.clone())
                });
            }
            agent.id = args[2].clone();
            agent.name = format!("{} (copy)", agent.name);
            agent.is_custom = true;
            agents.push(agent);
        }
        "remove" => {
            exact(args, 3, "agents remove ID --yes")?;
            if args[2] != "--yes" {
                bail!("agent removal requires --yes");
            }
            if !agents.iter().any(|agent| agent.id == args[1]) {
                bail!("unknown agent id");
            }
            // Mirror Studio reference cleanup before saving the new roster.
            cleanup_agent_references(project, &config, &args[1])?;
            agents.retain(|agent| agent.id != args[1]);
        }
        _ => bail!("usage: concerto {usage}"),
    }
    concerto_config::settings::save_settings_roster(&path, &agents)?;
    println!("Saved canonical agent files. Applies to the next run.");
    Ok(())
}

fn cleanup_agent_references(project: &Path, config: &AppConfig, id: &str) -> anyhow::Result<()> {
    let editor = editor(project, SettingsScope::Global)?;
    let mut edits = Vec::new();
    if let Some(multi) = &config.multi_agent {
        let pins: std::collections::HashMap<_, _> =
            multi.model_pins.iter().filter(|(key, _)| key.as_str() != id).collect();
        edits.push(("multi_agent.model_pins", typed_value(&pins)?));
        let relationships: Vec<_> = multi
            .relationships
            .iter()
            .filter(|relationship| relationship.from != id && relationship.to != id)
            .collect();
        edits.push(("multi_agent.relationships", typed_value(&relationships)?));
    }
    if let Some(settings) = &config.model_settings {
        let assignments: Vec<_> = settings
            .agent_assignments
            .iter()
            .filter(|assignment| assignment.agent_role != id)
            .collect();
        edits.push(("model_settings.agent_assignments", typed_value(&assignments)?));
    }
    if let Some(resolved) = &config.resolved_blueprint {
        let mut blueprint = resolved.blueprint.clone();
        for stage in &mut blueprint.pipeline.stages {
            stage.agents.retain(|agent| agent.as_str() != id);
        }
        blueprint
            .relationships
            .retain(|relationship| relationship.from != id && relationship.to != id);
        edits.push((
            "orchestration.blueprint",
            format!("{{ inline = {} }}", typed_value(&blueprint)?),
        ));
    }
    let edits: Vec<_> = edits.iter().map(|(key, value)| (*key, Some(value.as_str()))).collect();
    editor.edit_batch(&edits, false)?;
    Ok(())
}

pub fn run_blueprint(args: &[String], project: &Path) -> anyhow::Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("help");
    let usage = "blueprint <list|show|select NAME|import FILE>";
    match command {
        "help" => println!("{usage}\nBlueprints are advisory configuration; the Coordinator retains dispatch authority."),
        "list" => { exact(args, 1, "blueprint list")?; for name in concerto_config::NAMED_BLUEPRINTS { println!("{name}"); } }
        "show" => { exact(args, 1, "blueprint show")?; let config = global(project)?; let resolved = config.resolved_blueprint.context("no resolved blueprint")?; print!("{}", toml::to_string_pretty(&resolved.blueprint)?); }
        "select" => {
            exact(args, 2, "blueprint select NAME")?;
            if concerto_config::named_blueprint(&args[1]).is_none() { bail!("unknown named blueprint; run blueprint list"); }
            editor(project, SettingsScope::Global)?.edit("orchestration.blueprint", Some(&format!("{{ name = {} }}", typed_value(&args[1])?)), false)?;
            println!("Saved advisory blueprint selection. Applies to the next run.");
        }
        "import" => {
            exact(args, 2, "blueprint import FILE")?;
            let blueprint = concerto_config::parse_blueprint_file(Path::new(&args[1]))?;
            editor(project, SettingsScope::Global)?.edit("orchestration.blueprint", Some(&format!("{{ inline = {} }}", typed_value(&blueprint)?)), false)?;
            println!("Saved advisory blueprint. Applies to the next run.");
        }
        _ => bail!("usage: concerto {usage}"),
    }
    Ok(())
}

pub fn run_providers(args: &[String], project: &Path) -> anyhow::Result<()> {
    let usage = "providers <add ID TYPE [API_BASE]|remove ID --yes|refresh ID>";
    let config = global(project)?;
    let mut settings = config.model_settings.unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("add") => {
            if !(args.len() == 3 || args.len() == 4) {
                bail!("usage: concerto {usage}");
            }
            if args[1].is_empty()
                || settings.providers.iter().any(|provider| provider.id == args[1])
            {
                bail!("provider id is empty or already exists");
            }
            if !["openai", "anthropic", "google", "openrouter", "nim", "ollama", "opencode"]
                .contains(&args[2].as_str())
            {
                bail!("unknown provider type");
            }
            let provider = concerto_config::ProviderConfig {
                id: args[1].clone(),
                name: args[1].clone(),
                provider: args[2].clone(),
                api_base: args.get(3).cloned(),
                keyring_key: format!("providers/{}/api_key", args[1]),
                ..Default::default()
            };
            settings.providers.push(provider);
        }
        Some("remove") => {
            exact(args, 3, "providers remove ID --yes")?;
            if args[2] != "--yes" {
                bail!("provider removal requires --yes");
            }
            let provider = settings
                .providers
                .iter()
                .find(|provider| provider.id == args[1])
                .context("unknown provider id")?;
            if settings
                .agent_assignments
                .iter()
                .any(|assignment| assignment.provider_config_id == provider.id)
                || config.multi_agent.as_ref().is_some_and(|multi| {
                    multi
                        .custom_agents
                        .iter()
                        .any(|agent| agent.provider_id.as_deref() == Some(provider.id.as_str()))
                        || multi.default_provider_config_id.as_deref() == Some(provider.id.as_str())
                })
            {
                bail!(
                    "provider is assigned to an agent or fallback; change those assignments first"
                );
            }
            settings.providers.retain(|provider| provider.id != args[1]);
        }
        Some("refresh") => {
            exact(args, 2, "providers refresh ID")?;
            let provider = settings
                .providers
                .iter_mut()
                .find(|provider| provider.id == args[1])
                .context("unknown provider id")?;
            let key = if provider.provider == "ollama" {
                concerto_core::SecretString::from(String::new())
            } else {
                provider.api_key(&concerto_config::CredentialStore::new())?
            };
            let models = concerto_providers::list_models_for_provider_blocking(
                &provider.provider,
                key.expose(),
                provider.api_base.as_deref(),
            );
            if models.is_empty() {
                bail!("model discovery returned no models; existing catalog retained");
            }
            println!("Discovered {} models.", models.len());
            provider.record_discovered_models(models);
        }
        _ => bail!("usage: concerto {usage}"),
    }
    editor(project, SettingsScope::Global)?.edit(
        "model_settings.providers",
        Some(&typed_value(&settings.providers)?),
        false,
    )?;
    println!("Saved provider routes. Applies to the next run. Credentials are managed separately.");
    Ok(())
}

/// Read from a pipe for automation or use a hidden terminal prompt. Secret
/// values are never accepted as arguments or echoed to output.
fn read_credential(input: &mut impl Read) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    input.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        bail!("credential exceeds 16 KiB");
    }
    let value = String::from_utf8(bytes)?.trim_end_matches(['\r', '\n']).to_string();
    if value.trim().is_empty() || value.contains(['\r', '\n']) {
        bail!("credential must be one non-empty line");
    }
    Ok(value)
}

pub fn run_preferences(args: &[String]) -> anyhow::Result<()> {
    use concerto_memory::prefs::{PrefKey, UserPrefsStore};
    let usage = "preferences <show|set ui_theme NAME|set ui_font_size SIZE>";
    let directory =
        dirs::data_dir().context("cannot determine data directory")?.join("concerto").join("prefs");
    let store = UserPrefsStore::open(&directory)?;
    match args.first().map(String::as_str) {
        Some("show") => {
            exact(args, 1, "preferences show")?;
            print_value(serde_json::to_value(store.get_all())?)?;
        }
        Some("set") => {
            exact(args, 3, "preferences set <ui_theme|ui_font_size> VALUE")?;
            let key = match args[1].as_str() {
                "ui_theme" => {
                    if !crate::theme::CLI_THEME_NAMES
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&args[2]))
                    {
                        bail!("theme must be Midnight, Slate, Chalk, or Nebula");
                    }
                    PrefKey::UiTheme
                }
                "ui_font_size" => {
                    let size: f32 = args[2].parse().context("font size must be a number")?;
                    if !(12.0..=20.0).contains(&size) {
                        bail!("font size must be between 12 and 20");
                    }
                    PrefKey::UiFontSize
                }
                _ => bail!("usage: concerto {usage}"),
            };
            let value = if args[1] == "ui_theme" {
                crate::theme::CliTheme::by_name(&args[2]).name.to_string()
            } else {
                args[2].clone()
            };
            store.set(&key, value)?;
            println!("Saved desktop preference. Applies when Desktop next starts.");
        }
        _ => bail!("usage: concerto {usage}"),
    }
    Ok(())
}

pub fn run_credentials(args: &[String]) -> anyhow::Result<()> {
    let usage = "credentials <status ACCOUNT|set ACCOUNT --stdin|set ACCOUNT --prompt|delete ACCOUNT --yes>";
    let store = concerto_config::CredentialStore::new();
    match args.first().map(String::as_str) {
        Some("status") => {
            exact(args, 2, "credentials status ACCOUNT")?;
            println!("{}", if store.exists(&args[1]) { "present" } else { "missing" });
        }
        Some("set") => {
            exact(args, 3, "credentials set ACCOUNT <--stdin|--prompt>")?;
            let value = match args[2].as_str() {
                "--stdin" => read_credential(&mut io::stdin().lock())?,
                "--prompt" => {
                    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
                    crossterm::terminal::enable_raw_mode()?;
                    struct RawMode;
                    impl Drop for RawMode {
                        fn drop(&mut self) {
                            let _ = crossterm::terminal::disable_raw_mode();
                        }
                    }
                    let _guard = RawMode;
                    eprint!("API key (hidden): ");
                    io::stderr().flush()?;
                    let mut secret = String::new();
                    loop {
                        if let Event::Key(key) = read()? {
                            if key.kind != KeyEventKind::Press {
                                continue;
                            }
                            match key.code {
                                KeyCode::Enter => break,
                                KeyCode::Esc => bail!("credential entry cancelled"),
                                KeyCode::Char('c')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    bail!("credential entry cancelled")
                                }
                                KeyCode::Backspace => {
                                    secret.pop();
                                }
                                KeyCode::Char(ch)
                                    if !ch.is_control() && secret.len() < 16 * 1024 =>
                                {
                                    secret.push(ch)
                                }
                                _ => {}
                            }
                        }
                    }
                    eprintln!("\r");
                    read_credential(&mut secret.as_bytes())?
                }
                _ => bail!("secret values must use --stdin or --prompt"),
            };
            let secret = concerto_core::SecretString::from(value);
            store.set(&args[1], secret.expose())?;
            println!("Credential saved to the OS keychain.");
        }
        Some("delete") => {
            exact(args, 3, "credentials delete ACCOUNT --yes")?;
            if args[2] != "--yes" {
                bail!("credential deletion requires --yes");
            }
            store.delete(&args[1])?;
            println!("Credential deleted from the current keychain service.");
        }
        _ => bail!("usage: concerto {usage}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_flags_reject_conflicts_and_unknown_options() {
        let flags = vec!["--global".into(), "--project-scope".into()];
        assert!(selected_scope(&flags).is_err());
        assert!(flag_args(&["--typo".into()], &["--global"]).is_err());
        assert_eq!(flag_args(&["--".into(), "--literal".into()], &[]).unwrap().0, ["--literal"]);
    }

    #[test]
    fn credential_input_accepts_newline_terminated_pipes_and_rejects_multiple_lines() {
        assert_eq!(read_credential(&mut "synthetic\r\n".as_bytes()).unwrap(), "synthetic");
        assert!(read_credential(&mut "a\nb".as_bytes()).is_err());
        assert!(read_credential(&mut "\n".as_bytes()).is_err());
        assert!(read_credential(&mut vec![b'a'; 16 * 1024 + 1].as_slice()).is_err());
    }

    #[test]
    fn keys_expose_optional_shared_settings_and_provider_record_fields() {
        let value = serde_json::to_value(concerto_config::settings::settings_example()).unwrap();
        let mut keys = Vec::new();
        setting_keys(&value, "", &mut keys);
        for key in [
            "retry.max_attempts",
            "memory.ttl_days",
            "shell_settings",
            "orchestration.blueprint.name",
            "model_settings.providers.0.timeout_seconds",
            "project_context.enabled",
            "mcp.servers.0.timeout_secs",
            "policy.time_window.timezone",
        ] {
            assert!(
                keys.iter()
                    .any(|candidate| candidate == key || candidate.starts_with(&format!("{key}."))),
                "{key}"
            );
        }
    }
}
