//! Native shell and security editing for the human CLI client.
use async_trait::async_trait;
use concerto_core::shell_security::ShellSecurity;
use concerto_core::types::PolicyAction;
use concerto_core::{ApprovalDecision, ApprovalSink, CancellationToken};
use concerto_shell::CommandInvocation;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

type Input = Arc<Mutex<mpsc::Receiver<String>>>;
struct ConsoleApproval {
    input: Input,
}
#[async_trait]
impl ApprovalSink for ConsoleApproval {
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> ApprovalDecision {
        eprintln!(
            "Approval required for {}: {:?}",
            action.tool_name,
            action.command_facts.as_ref().map(|f| (&f.argv, &f.working_directory))
        );
        eprintln!("Type approve to run this operation, or anything else to deny:");
        let mut input = self.input.lock().await;
        tokio::select! {
            _ = cancel.cancelled() => ApprovalDecision::Deny,
            answer = input.recv() => if answer.is_some_and(|a| a.trim() == "approve") { ApprovalDecision::Approve } else { ApprovalDecision::Deny },
        }
    }
    async fn approve_all_for_session(
        &self,
        _session: concerto_core::ids::Ulid,
        _cancel: CancellationToken,
    ) {
    }
    async fn request_ack(
        &self,
        _session: concerto_core::ids::Ulid,
        _message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        false
    }
}

pub fn run(args: &[String], project: &Path) -> anyhow::Result<()> {
    if args.first().map(String::as_str) == Some("security") {
        return security(&args[1..]);
    }
    if !args.is_empty() && args[0] != "exec" {
        anyhow::bail!("usage: concerto --cli shell [exec COMMAND ARG ... | security show | security validate FILE | security apply FILE]");
    }
    let config = concerto_config::load_config(None, Some(project))?;
    let (sender, receiver) = mpsc::channel(1);
    // A detached stdin reader lets cancellation end the async runtime without
    // waiting for an uncancellable spawn_blocking read.
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if sender.blocking_send(line).is_err() {
                break;
            }
        }
    });
    let input = Arc::new(Mutex::new(receiver));
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let signal_task = tokio::spawn(async move { if tokio::signal::ctrl_c().await.is_ok() { signal.cancel(); } });
        let shell = concerto_shell::client::open(project.to_path_buf(), config, Arc::new(ConsoleApproval { input: input.clone() })).await.map_err(anyhow::Error::msg)?;
        let result = async {
            if args.first().map(String::as_str) == Some("exec") {
                let Some(command) = args.get(1) else { anyhow::bail!("shell exec requires a native command"); };
                let result = shell.execute(CommandInvocation { command: command.clone(), arguments: args[2..].to_vec(), raw: String::new() }, cancel.clone()).await;
                println!("{}", serde_json::to_string_pretty(&result)?);
                if !result.status.is_success() { anyhow::bail!("{}", result.summary); }
                return Ok(());
            }
            println!("Concerto native shell. Type help or exit. Ctrl-C cancels and exits.");
            loop {
                print!("concerto> "); io::stdout().flush()?;
                let mut receiver = input.lock().await;
                let line = tokio::select! { _ = cancel.cancelled() => None, line = receiver.recv() => line };
                drop(receiver);
                let Some(line) = line else { break; };
                if line.trim() == "exit" { break; }
                if line.trim().is_empty() { continue; }
                let result = shell.execute_line(&line, cancel.clone()).await;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Ok::<(), anyhow::Error>(())
        }.await;
        signal_task.abort();
        result
    })
}

fn security(args: &[String]) -> anyhow::Result<()> {
    let path = concerto_config::default_config_path()
        .ok_or_else(|| anyhow::anyhow!("global config path is unavailable"))?;
    let current = concerto_config::load_global_config(Some(&path))?.shell_security;
    match args.first().map(String::as_str) {
        None | Some("show") => println!("{}", serde_json::to_string_pretty(&current)?),
        Some("validate" | "apply") => {
            let file = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("security validate/apply requires a JSON file"))?;
            let mut proposed: ShellSecurity =
                serde_json::from_str(&std::fs::read_to_string(file)?)?;
            proposed.revision = current.revision;
            proposed.validate().map_err(anyhow::Error::msg)?;
            if args[0] == "validate" {
                println!("Security settings are valid.");
                return Ok(());
            }
            let before = serde_json::to_value(&current)?;
            let after = serde_json::to_value(&proposed)?;
            if let Some(fields) = after.as_object() {
                for (key, value) in fields {
                    if before.get(key) != Some(value) {
                        println!(
                            "{key}: {} -> {value}",
                            before.get(key).unwrap_or(&serde_json::Value::Null)
                        );
                    }
                }
            }
            println!("Host execution uses ambient OS permissions. Existing runs keep their captured settings. Type apply to save these security changes:");
            let mut confirmation = String::new();
            io::stdin().read_line(&mut confirmation)?;
            if confirmation.trim() != "apply" {
                anyhow::bail!("security changes cancelled");
            }
            let saved = concerto_config::shell_security::save_shell_security(
                &path,
                current.revision,
                proposed,
            )?;
            println!(
                "Saved security revision {}. Restart the console and active runs to apply it.",
                saved.revision
            );
        }
        _ => anyhow::bail!("usage: shell security show | validate FILE | apply FILE"),
    }
    Ok(())
}
