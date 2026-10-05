//! Portable file commands. Every effect passes through the central tool executor.
use crate::{
    CommandEffect, CommandInvocation, CommandResult, CommandServices, CommandSource, CommandSpec,
    PolicyExecutionAdapter, ShellCommand, ShellContext,
};
use async_trait::async_trait;
use concerto_core::CancellationToken;
use serde_json::json;
use std::sync::Arc;

pub(crate) fn commands(adapter: PolicyExecutionAdapter) -> Vec<Arc<dyn ShellCommand>> {
    [
        ("cat", "read", 1),
        ("ls", "list", 1),
        ("write", "write", 2),
        ("rm", "delete", 1),
        ("cp", "copy", 2),
        ("mv", "move", 2),
    ]
    .into_iter()
    .map(|(name, operation, operands)| {
        Arc::new(FileCommand {
            spec: CommandSpec {
                name: name.into(),
                description: format!("Native filesystem {operation} through Concerto policy"),
                usage: match operation {
                    "write" => "write PATH CONTENT".into(),
                    "copy" | "move" => format!("{name} SOURCE DESTINATION"),
                    _ => format!("{name} PATH"),
                },
                source: CommandSource::Builtin,
                effects: vec![if matches!(operation, "read" | "list") {
                    CommandEffect::ProjectRead
                } else {
                    CommandEffect::ProjectWrite
                }],
                records_history: false,
            },
            operation,
            operands,
            adapter: adapter.clone(),
        }) as Arc<dyn ShellCommand>
    })
    .collect()
}

struct FileCommand {
    spec: CommandSpec,
    operation: &'static str,
    operands: usize,
    adapter: PolicyExecutionAdapter,
}
#[async_trait]
impl ShellCommand for FileCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }
    async fn execute(
        &self,
        invocation: &CommandInvocation,
        _context: &ShellContext,
        _services: &CommandServices,
        cancel: CancellationToken,
    ) -> CommandResult {
        if invocation.arguments.len() != self.operands {
            return CommandResult::recoverable(&self.spec.name, "shell.usage", &self.spec.usage);
        }
        let mut input = json!({"operation":self.operation,"path":invocation.arguments[0]});
        if self.operands == 2 {
            input[if self.operation == "write" { "content" } else { "destination" }] =
                json!(invocation.arguments[1]);
        }
        self.adapter.execute_tool(&self.spec.name, "filesystem", input, cancel).await
    }
}
