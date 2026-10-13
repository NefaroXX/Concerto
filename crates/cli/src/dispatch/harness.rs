//! `concerto harness` — JSON/CLI access to the H13 harness service.
//!
//! Every subcommand builds the ONE harness service
//! ([`concerto_orchestrator::harness_service::build_standalone_harness`]) and
//! invokes an operation on it. The service is the same seam the desktop and
//! in-process paths call; the CLI never touches a tool, a checkpoint store, or
//! a memory database directly.
//!
//! Human-readable by default; `--json` emits the service invocation
//! ([`HarnessInvocation`]) as compact JSON — the service's shape, not a second
//! serialization. This mirrors `concerto health --json`.

use std::path::Path;

use concerto_orchestrator::harness_service::{
    build_standalone_harness, ApproveInvocationSink, HarnessInvocation, HarnessServiceError,
};
use serde_json::json;

/// Usage line echoed on every `harness` argument error.
const HARNESS_USAGE: &str =
    "usage: concerto harness <environment|verify|checkpoint|memory> [--json]\n\
     environment [--detail summary|full]\n\
     verify [--scope PATH] [--verifier NAME]\n\
     checkpoint create --path P [--path P] ...\n\
     checkpoint inspect <id>\n\
     checkpoint restore <id>\n\
     checkpoint prune [--retention-days N]\n\
     memory recall <query> [--top-k N]\n\
     memory inspect <chunk-id>";

/// Parsed global `--json` flag plus the remaining positional/option tokens.
struct Parsed {
    json: bool,
    rest: Vec<String>,
}

/// Strip a trailing `--json` (and `--json` anywhere) from an argument list.
fn take_json(args: &[String]) -> Parsed {
    let mut json = false;
    let mut rest = Vec::new();
    for arg in args {
        if arg == "--json" {
            json = true;
        } else {
            rest.push(arg.clone());
        }
    }
    Parsed { json, rest }
}

/// Run `concerto harness ...`.
pub(crate) fn run_harness_subcommand(args: &[String], project_root: &Path) -> anyhow::Result<()> {
    let parsed = take_json(args);
    let invocation = invoke_harness(&parsed.rest, project_root)?;
    if parsed.json {
        println!("{}", serde_json::to_string(&invocation)?);
    } else {
        print_human(&invocation);
    }
    Ok(())
}

/// Build the harness service and invoke one operation — the testable core of
/// the CLI adapter (no stdout).
pub(crate) fn invoke_harness(
    args: &[String],
    project_root: &Path,
) -> anyhow::Result<HarnessInvocation> {
    let Some(group) = args.first().map(String::as_str) else {
        anyhow::bail!("{HARNESS_USAGE}");
    };
    let config_path = concerto_config::default_config_path();
    let config = concerto_config::load_config(config_path.as_ref(), Some(project_root))?;

    let (operation, input, memory_enabled) = match group {
        "environment" => parse_environment(&args[1..])?,
        "verify" => parse_verify(&args[1..])?,
        "checkpoint" => parse_checkpoint(&args[1..])?,
        "memory" => parse_memory(&args[1..])?,
        other => anyhow::bail!("unknown harness group '{other}'\n{HARNESS_USAGE}"),
    };

    // `checkpoint.prune` is an operator maintenance action (not one of the six
    // service operations); it runs through the sessions store's retention seam.
    if operation == "checkpoint.prune" {
        return run_checkpoint_prune(&config, project_root, &input);
    }

    let rt = tokio::runtime::Runtime::new()?;
    // The `StandaloneHarness` is dropped inside the async block (its `Drop`
    // cancels any memory background tasks) before the runtime drops.
    let invocation = rt.block_on(async {
        let harness = build_standalone_harness(
            project_root,
            &config,
            memory_enabled,
            std::sync::Arc::new(ApproveInvocationSink),
        )
        .await?;
        harness
            .service
            .invoke(&operation, input, concerto_core::CancellationToken::new())
            .await
            .map_err(|error| anyhow::anyhow!(render_service_error(&operation, &error)))
    })?;
    Ok(invocation)
}

/// Render a service error with the operation name (a refusal is never silent).
fn render_service_error(operation: &str, error: &HarnessServiceError) -> String {
    format!("harness {operation} failed: {error}")
}

/// Parse `environment [--detail summary|full]`.
fn parse_environment(args: &[String]) -> anyhow::Result<(String, serde_json::Value, bool)> {
    let mut detail = "summary".to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--detail" => {
                index += 1;
                detail = args
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--detail requires a value\n{HARNESS_USAGE}"))?;
            }
            other => anyhow::bail!("unknown environment option '{other}'\n{HARNESS_USAGE}"),
        }
        index += 1;
    }
    Ok((
        concerto_orchestrator::environment_describe::ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string(),
        json!({ "detail": detail }),
        false,
    ))
}

/// Parse `verify [--scope PATH] [--verifier NAME]`.
fn parse_verify(args: &[String]) -> anyhow::Result<(String, serde_json::Value, bool)> {
    let mut scope: Option<String> = None;
    let mut verifier: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--scope" => {
                index += 1;
                scope =
                    Some(args.get(index).cloned().ok_or_else(|| {
                        anyhow::anyhow!("--scope requires a value\n{HARNESS_USAGE}")
                    })?);
            }
            "--verifier" => {
                index += 1;
                verifier = Some(args.get(index).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--verifier requires a value\n{HARNESS_USAGE}")
                })?);
            }
            other => anyhow::bail!("unknown verify option '{other}'\n{HARNESS_USAGE}"),
        }
        index += 1;
    }
    let mut input = json!({});
    if let Some(scope) = scope {
        input["scope"] = json!(scope);
    }
    if let Some(verifier) = verifier {
        input["verifier"] = json!(verifier);
    }
    Ok((concerto_orchestrator::verify::VERIFY_RUN_TOOL_NAME.to_string(), input, false))
}

/// Parse `checkpoint <create|inspect|restore> ...`.
fn parse_checkpoint(args: &[String]) -> anyhow::Result<(String, serde_json::Value, bool)> {
    use concerto_orchestrator::harness_checkpoint::{
        CHECKPOINT_CREATE_TOOL_NAME, CHECKPOINT_INSPECT_TOOL_NAME, CHECKPOINT_RESTORE_TOOL_NAME,
    };
    let Some(action) = args.first().map(String::as_str) else {
        anyhow::bail!("{HARNESS_USAGE}");
    };
    match action {
        "create" => {
            let mut paths = Vec::new();
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--path" => {
                        index += 1;
                        paths.push(args.get(index).cloned().ok_or_else(|| {
                            anyhow::anyhow!("--path requires a value\n{HARNESS_USAGE}")
                        })?);
                    }
                    other => {
                        anyhow::bail!("unknown checkpoint create option '{other}'\n{HARNESS_USAGE}")
                    }
                }
                index += 1;
            }
            if paths.is_empty() {
                anyhow::bail!("checkpoint create requires at least one --path\n{HARNESS_USAGE}");
            }
            Ok((CHECKPOINT_CREATE_TOOL_NAME.to_string(), json!({ "paths": paths }), false))
        }
        "inspect" => {
            let id = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("checkpoint inspect requires a checkpoint id\n{HARNESS_USAGE}")
            })?;
            Ok((CHECKPOINT_INSPECT_TOOL_NAME.to_string(), json!({ "checkpoint_id": id }), false))
        }
        "restore" => {
            let id = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("checkpoint restore requires a checkpoint id\n{HARNESS_USAGE}")
            })?;
            Ok((CHECKPOINT_RESTORE_TOOL_NAME.to_string(), json!({ "checkpoint_id": id }), false))
        }
        "prune" => {
            let mut input = json!({});
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--retention-days" => {
                        index += 1;
                        let raw = args.get(index).ok_or_else(|| {
                            anyhow::anyhow!("--retention-days requires a value\n{HARNESS_USAGE}")
                        })?;
                        let days: u64 = raw.parse().map_err(|_| {
                            anyhow::anyhow!("invalid --retention-days value '{raw}'")
                        })?;
                        input["retention_days"] = json!(days);
                    }
                    other => {
                        anyhow::bail!("unknown checkpoint prune option '{other}'\n{HARNESS_USAGE}")
                    }
                }
                index += 1;
            }
            Ok(("checkpoint.prune".to_string(), input, false))
        }
        other => anyhow::bail!("unknown checkpoint action '{other}'\n{HARNESS_USAGE}"),
    }
}

/// `checkpoint.prune`: operator-invoked retention over the checkpoint sidecar,
/// applying `[harness].checkpoint_retention_days` unless `--retention-days`
/// overrides it. Never runs automatically — H13 wires the setting to this
/// explicit action, and nothing else.
fn run_checkpoint_prune(
    config: &concerto_config::AppConfig,
    _project_root: &Path,
    input: &serde_json::Value,
) -> anyhow::Result<HarnessInvocation> {
    let retention_days = input
        .get("retention_days")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| config.harness.as_ref().and_then(|harness| harness.checkpoint_retention_days))
        .filter(|days| *days > 0);
    let Some(retention_days) = retention_days else {
        return Ok(HarnessInvocation {
            operation: "checkpoint.prune".to_string(),
            summary: "checkpoint.prune skipped: no retention window configured".to_string(),
            data: json!({
                "pruned": false,
                "reason": "no retention window (set [harness].checkpoint_retention_days or pass --retention-days)",
            }),
        });
    };
    let rt = tokio::runtime::Runtime::new()?;
    let report = rt.block_on(async {
        let store = concerto_sessions::SqliteSessionStore::connect().await?;
        store.prune_file_checkpoints(retention_days, &concerto_core::CancellationToken::new()).await
    })?;
    Ok(HarnessInvocation {
        operation: "checkpoint.prune".to_string(),
        summary: format!(
            "checkpoint.prune retention_days={retention_days} expired={} sidecar_rows_deleted={}",
            report.expired, report.sidecar_rows_deleted,
        ),
        data: json!({
            "retention_days": retention_days,
            "expired": report.expired,
            "sidecar_rows_deleted": report.sidecar_rows_deleted,
            "cutoff_unix": report.cutoff_unix,
        }),
    })
}

/// Parse `memory <recall|inspect> ...`.
fn parse_memory(args: &[String]) -> anyhow::Result<(String, serde_json::Value, bool)> {
    use concerto_orchestrator::harness_memory::{
        MEMORY_INSPECT_TOOL_NAME, MEMORY_RECALL_TOOL_NAME,
    };
    let Some(action) = args.first().map(String::as_str) else {
        anyhow::bail!("{HARNESS_USAGE}");
    };
    match action {
        "recall" => {
            let query = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("memory recall requires a query\n{HARNESS_USAGE}")
            })?;
            let mut input = json!({ "query": query });
            if let Some(top_k) = args.get(2) {
                let top_k: u32 = top_k
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid --top-k value '{top_k}'"))?;
                input["top_k"] = json!(top_k);
            }
            Ok((MEMORY_RECALL_TOOL_NAME.to_string(), input, true))
        }
        "inspect" => {
            let chunk_id = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("memory inspect requires a chunk id\n{HARNESS_USAGE}")
            })?;
            Ok((MEMORY_INSPECT_TOOL_NAME.to_string(), json!({ "chunk_id": chunk_id }), true))
        }
        other => anyhow::bail!("unknown memory action '{other}'\n{HARNESS_USAGE}"),
    }
}

/// Human-readable rendering of one invocation (the compact, non-`--json`
/// default). Surfaces refusal/partial/degraded states honestly.
fn print_human(invocation: &HarnessInvocation) {
    println!("operation: {}", invocation.operation);
    println!("{}", invocation.summary);
    if invocation.is_degraded() {
        println!("degraded: true (reduced capability)");
    }
    if invocation.is_partial() {
        println!("partial: true (see payload for missing/limitations)");
    }
    // Print the payload as pretty JSON so no field is hidden from the operator
    // (the machine shape is identical to `--json`).
    match serde_json::to_string_pretty(&invocation.data) {
        Ok(pretty) => println!("{pretty}"),
        Err(error) => eprintln!("could not render payload: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn take_json_extracts_the_flag_from_anywhere() {
        let parsed = take_json(&args(&["memory", "--json", "recall", "x"]));
        assert!(parsed.json);
        assert_eq!(parsed.rest, args(&["memory", "recall", "x"]));
        let parsed = take_json(&args(&["environment"]));
        assert!(!parsed.json);
    }

    #[test]
    fn environment_parses_detail_and_flags_memory_off() {
        let (op, input, memory) = parse_environment(&args(&[])).unwrap();
        assert_eq!(op, "environment.describe");
        assert_eq!(input["detail"], "summary");
        assert!(!memory, "environment needs no memory subsystem");
        let (_, input, _) = parse_environment(&args(&["--detail", "full"])).unwrap();
        assert_eq!(input["detail"], "full");
        assert!(parse_environment(&args(&["--bogus"])).is_err());
    }

    #[test]
    fn checkpoint_create_requires_a_path() {
        assert!(parse_checkpoint(&args(&["create"])).is_err());
        let (op, input, memory) =
            parse_checkpoint(&args(&["create", "--path", "a.rs", "--path", "b.rs"])).unwrap();
        assert_eq!(op, "checkpoint.create");
        assert_eq!(input["paths"], json!(["a.rs", "b.rs"]));
        assert!(!memory);
    }

    #[test]
    fn checkpoint_inspect_and_restore_carry_the_id() {
        let (op, input, _) = parse_checkpoint(&args(&["inspect", "cp-1"])).unwrap();
        assert_eq!(op, "checkpoint.inspect");
        assert_eq!(input["checkpoint_id"], "cp-1");
        let (op, _, _) = parse_checkpoint(&args(&["restore", "cp-1"])).unwrap();
        assert_eq!(op, "checkpoint.restore");
        assert!(parse_checkpoint(&args(&["restore"])).is_err());
    }

    #[test]
    fn memory_recall_enables_memory_and_parses_top_k() {
        let (op, input, memory) = parse_memory(&args(&["recall", "why", "7"])).unwrap();
        assert_eq!(op, "memory.recall");
        assert_eq!(input["query"], "why");
        assert_eq!(input["top_k"], 7);
        assert!(memory, "memory commands need the memory subsystem");
        assert!(parse_memory(&args(&["recall", "x", "not-a-number"])).is_err());
        assert!(parse_memory(&args(&["inspect", "chunk-1"])).is_ok());
    }

    #[test]
    fn verify_scope_and_verifier_ride_the_input() {
        let (op, input, memory) =
            parse_verify(&args(&["--scope", "src/lib.rs", "--verifier", "build"])).unwrap();
        assert_eq!(op, "verify.run");
        assert_eq!(input["scope"], "src/lib.rs");
        assert_eq!(input["verifier"], "build");
        assert!(!memory);
    }

    #[test]
    fn unknown_group_and_action_are_errors() {
        assert!(parse_checkpoint(&args(&["nope"])).is_err());
        assert!(parse_memory(&args(&["nope"])).is_err());
    }

    #[test]
    fn checkpoint_prune_parses_and_skips_without_a_window() {
        let (op, input, memory) =
            parse_checkpoint(&args(&["prune", "--retention-days", "14"])).unwrap();
        assert_eq!(op, "checkpoint.prune");
        assert_eq!(input["retention_days"], 14);
        assert!(!memory);
        // No `[harness]` window and no flag: skip without touching the store.
        let invocation = run_checkpoint_prune(
            &concerto_config::AppConfig::default(),
            std::path::Path::new("."),
            &serde_json::json!({}),
        )
        .expect("prune skip is not an error");
        assert_eq!(invocation.data["pruned"], false);
    }

    /// RAII guard restoring `XDG_DATA_HOME` even on panic (the CLI service
    /// builder opens the real app data dir).
    struct DataHomeGuard {
        previous_data: Option<String>,
        previous_config: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl DataHomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir for XDG dirs");
            let previous_data = std::env::var("XDG_DATA_HOME").ok();
            let previous_config = std::env::var("XDG_CONFIG_HOME").ok();
            std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
            Self { previous_data, previous_config, _dir: dir }
        }
    }

    impl Drop for DataHomeGuard {
        fn drop(&mut self) {
            match &self.previous_data {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            match &self.previous_config {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    /// End-to-end CLI adapter: `environment.describe` builds the SAME service
    /// the desktop/in-process paths use and returns its payload. No memory
    /// subsystem, so this never touches the embedder.
    #[test]
    fn cli_environment_invokes_the_shared_service() {
        let _guard = crate::tests::ENV_LOCK.lock().unwrap();
        let _data = DataHomeGuard::new();
        let project = tempfile::tempdir().expect("project dir");
        let invocation =
            invoke_harness(&args(&["environment", "--detail", "full"]), project.path())
                .expect("environment describe runs through the service");
        assert_eq!(invocation.operation, "environment.describe");
        // The `full` detail renders the bounded manifest body: a tool array
        // with truncation flags (the `summary` detail would not).
        assert!(invocation.data["tools"].is_array());
        assert!(invocation.data["total_tools"].is_number());
        assert!(invocation.data["run_id"].is_string());
        // The `--json` emission is exactly the service shape, not a second
        // serialization: round-tripping the printed JSON yields the same value.
        let json = serde_json::to_string(&invocation).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(value["operation"], "environment.describe");
        assert_eq!(value["data"], invocation.data);
    }

    /// End-to-end CLI adapter for the checkpoint operations (no embedder): the
    /// create payload is a real active checkpoint, and inspect reads it back
    /// through the same service seam.
    #[test]
    fn cli_checkpoint_create_then_inspect_share_the_service_payloads() {
        let _guard = crate::tests::ENV_LOCK.lock().unwrap();
        let _data = DataHomeGuard::new();
        let project = tempfile::tempdir().expect("project dir");
        std::fs::write(project.path().join("a.txt"), "hello\n").expect("seed file");

        let created =
            invoke_harness(&args(&["checkpoint", "create", "--path", "a.txt"]), project.path())
                .expect("checkpoint create runs through the service");
        assert_eq!(created.operation, "checkpoint.create");
        assert_eq!(created.data["status"], "active");
        let id = created.data["checkpoint_id"].as_str().expect("checkpoint id").to_string();

        let inspected = invoke_harness(&args(&["checkpoint", "inspect", &id]), project.path())
            .expect("checkpoint inspect runs through the service");
        assert_eq!(inspected.operation, "checkpoint.inspect");
        assert_eq!(inspected.data["found"], true);
        assert_eq!(inspected.data["record"]["checkpoint_id"].as_str(), Some(id.as_str()));
        // The JSON shape is the service shape, byte-for-byte serializable.
        let json = serde_json::to_string(&inspected).expect("serializes");
        assert!(json.contains("\"operation\":\"checkpoint.inspect\""));
    }
}
