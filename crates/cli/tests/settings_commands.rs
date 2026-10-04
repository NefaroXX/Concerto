#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        Self { dir }
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_concerto-cli"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env_remove("CONCERTO_PROJECT_ROOTS")
            .args(["--project", self.dir.path().join("project").to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> String {
        let output = self.command(args);
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    }

    fn config_path(&self) -> std::path::PathBuf {
        self.dir.path().join("config/concerto/config.toml")
    }
}

#[test]
fn scoped_edits_and_dry_runs_validate_without_setup_or_provider() {
    let fixture = Fixture::new();
    fixture.success(&["config", "set", "retry.max_attempts", "4", "--dry-run"]);
    assert!(!fixture.config_path().exists());
    fixture.success(&["config", "set", "memory.enabled", "true"]);
    let global = std::fs::read_to_string(fixture.config_path()).unwrap();
    fixture.success(&["config", "set", "memory.enabled", "false", "--project-scope"]);
    assert_eq!(fixture.success(&["config", "get", "memory.enabled"]).trim(), "false");
    assert_eq!(fixture.success(&["config", "get", "memory.enabled", "--global"]).trim(), "true");
    for args in [
        vec!["config", "set", "retry.max_attempts", "0"],
        vec!["config", "set", "retry.typo", "1"],
        vec!["config", "set", "memory.enabled", "true", "--typo"],
        vec!["config", "set", "orchestration.blueprint.name", "'tdd'", "--project-scope"],
    ] {
        assert!(!fixture.command(&args).status.success(), "{args:?}");
    }
    assert_eq!(std::fs::read_to_string(fixture.config_path()).unwrap(), global);
    fixture.success(&["config", "unset", "memory.enabled", "--project-scope"]);
    assert_eq!(fixture.success(&["config", "get", "memory.enabled"]).trim(), "true");
    assert!(fixture.success(&["config", "show", "--example", "--json"]).contains("retry"));
}

#[test]
fn agent_edits_clone_and_removal_use_the_authoritative_roster() {
    let fixture = Fixture::new();
    fixture.success(&["agents", "set", "coder", "capabilities.shell", "false"]);
    let coder = fixture.config_path().parent().unwrap().join("agents/coder.toml");
    assert!(std::fs::read_to_string(&coder).unwrap().contains("shell = false"));
    let before = std::fs::read_to_string(&coder).unwrap();
    assert!(!fixture
        .command(&["agents", "set", "coder", "capabilities.typo", "true"])
        .status
        .success());
    assert_eq!(std::fs::read_to_string(&coder).unwrap(), before);
    fixture.success(&["agents", "clone", "coder", "code-copy"]);
    assert!(fixture.success(&["agents", "show", "code-copy"]).contains("code-copy"));
    assert!(!fixture.command(&["agents", "remove", "code-copy"]).status.success());
    fixture.success(&["agents", "remove", "code-copy", "--yes"]);
    assert!(!fixture.command(&["agents", "show", "code-copy"]).status.success());
    assert!(fixture.success(&["agents", "list"]).contains("coder"));
}

#[test]
fn extension_records_preserve_argument_boundaries_and_redact_environment() {
    let fixture = Fixture::new();
    let server = fixture.dir.path().join("server.toml");
    // Credential-like env keys must be keyring references (ADR-78); settings
    // inspection redacts whatever value the record stores.
    std::fs::write(
        &server,
        "id = 'demo'\ncommand = 'demo'\nargs = ['two words']\n[env]\nTOKEN = 'keyring:mcp/demo/TOKEN'\n",
    )
    .unwrap();
    fixture.success(&["extensions", "mcp", "add", server.to_str().unwrap()]);
    fixture.success(&["extensions", "mcp", "disable", "demo"]);
    fixture.success(&["extensions", "mcp", "set", "demo", "timeout_secs", "1234"]);
    let output = fixture.success(&["extensions", "mcp", "show", "demo"]);
    assert!(
        output.contains("two words") && output.contains("1234") && output.contains("[REDACTED]")
    );
    assert!(!output.contains("keyring:mcp/demo/TOKEN"));
    let output = fixture.success(&["config", "get", "mcp.servers.0.env.TOKEN"]);
    assert!(!output.contains("keyring:mcp/demo/TOKEN"));
    fixture.success(&["extensions", "mcp", "remove", "demo", "--yes"]);
    assert!(!fixture.success(&["extensions", "mcp", "list"]).contains("demo"));
    std::fs::write(&server, "id = 'plain'\ncommand = 'demo'\n[env]\nTOKEN = 'synthetic-secret'\n")
        .unwrap();
    assert!(!fixture
        .command(&["extensions", "mcp", "add", server.to_str().unwrap()])
        .status
        .success());
    std::fs::write(&server, "id = 'bad'\ncommand = 'demo'\ntimeout_ms = 1\n").unwrap();
    assert!(!fixture
        .command(&["extensions", "mcp", "add", server.to_str().unwrap()])
        .status
        .success());
}

#[test]
fn skills_lifecycle_uses_the_same_pack_files_and_recoverable_removal() {
    let fixture = Fixture::new();
    let manifest = fixture.dir.path().join("skill.toml");
    let packs = fixture.dir.path().join("packs");
    std::fs::write(&manifest, "id = 'demo-pack'\nname = 'Demo'\nversion = '1.0.0'\ndescription = 'Fixture'\ninstructions = 'Original instruction'\n").unwrap();
    fixture.success(&[
        "extensions",
        "skills",
        "create",
        manifest.to_str().unwrap(),
        packs.to_str().unwrap(),
    ]);
    fixture.success(&["extensions", "skills", "disable", "demo-pack"]);
    assert!(!fixture.success(&["config", "get", "skills.enabled_ids"]).contains("demo-pack"));
    fixture.success(&["extensions", "skills", "enable", "demo-pack"]);
    assert!(fixture.success(&["config", "get", "skills.enabled_ids"]).contains("demo-pack"));
    std::fs::write(&manifest, "id = 'demo-pack'\nname = 'Demo'\nversion = '1.0.0'\ndescription = 'Fixture'\ninstructions = 'Updated instruction'\n").unwrap();
    fixture.success(&["extensions", "skills", "edit", "demo-pack", manifest.to_str().unwrap()]);
    assert!(fixture
        .success(&["extensions", "skills", "show", "demo-pack"])
        .contains("Updated instruction"));
    fixture.success(&["extensions", "skills", "remove", "demo-pack", "--yes"]);
    assert!(!fixture.success(&["extensions", "skills", "list"]).contains("demo-pack"));
    assert!(std::fs::read_dir(packs.join("demo-pack")).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".deleted-")));
}

#[test]
fn explicit_mcp_probe_connects_and_stops_without_persisting_enable_switches() {
    let fixture = Fixture::new();
    let script = fixture.dir.path().join("mcp_fixture.py");
    std::fs::write(&script, r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    if request['method'] == 'initialize':
        result = {'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1.0.0'}}
    elif request['method'] == 'tools/list':
        result = {'tools':[{'name':'fixture_tool','description':'Fixture','inputSchema':{'type':'object','properties':{}}}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#).unwrap();
    let server = fixture.dir.path().join("probe.toml");
    std::fs::write(
        &server,
        format!(
            "id = 'probe'\ncommand = 'python3'\nargs = [{}]\nenabled = false\ntimeout_secs = 3\n",
            toml::Value::String(script.to_string_lossy().into())
        ),
    )
    .unwrap();
    fixture.success(&["extensions", "mcp", "add", server.to_str().unwrap()]);
    let before = std::fs::read_to_string(fixture.config_path()).unwrap();
    assert!(fixture.success(&["extensions", "mcp", "probe", "probe"]).contains("fixture_tool"));
    assert_eq!(std::fs::read_to_string(fixture.config_path()).unwrap(), before);
}

#[test]
fn providers_blueprints_and_desktop_preferences_are_cli_configurable() {
    let fixture = Fixture::new();
    fixture.success(&["providers", "add", "local", "ollama", "http://localhost:11434"]);
    fixture.success(&["config", "set", "model_settings.providers.0.model", "'example-model'"]);
    assert!(fixture.success(&["providers", "list"]).contains("example-model"));
    fixture.success(&["blueprint", "select", "tdd"]);
    let blueprint = fixture.success(&["blueprint", "show"]);
    assert!(blueprint.contains("tdd"));
    let imported = fixture.dir.path().join("blueprint.toml");
    std::fs::write(&imported, blueprint).unwrap();
    fixture.success(&["blueprint", "import", imported.to_str().unwrap()]);
    fixture.success(&["preferences", "set", "ui_font_size", "16"]);
    fixture.success(&["preferences", "set", "ui_theme", "Slate"]);
    assert!(fixture.success(&["preferences", "show"]).contains("Slate"));
    assert!(!fixture.command(&["preferences", "set", "ui_font_size", "NaN"]).status.success());
    let prefs = fixture.dir.path().join("data/concerto/prefs/user_prefs.json");
    assert!(Path::new(&prefs).exists());
}
