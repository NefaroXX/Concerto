use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
use concerto_plugins::capability::{CapabilityApprovalUI, GrantDecision};
use concerto_plugins::error::PluginError;
use std::io::{self, Write};

/// CLI implementation of CapabilityApprovalUI.
///
/// In interactive mode, prints a prompt and reads user input.
/// In non-interactive mode, auto-denies all capability requests.
pub struct CliPluginApproval {
    interactive: bool,
}

impl CliPluginApproval {
    pub fn new(interactive: bool) -> Self {
        Self { interactive }
    }
}

/// Format a capability request into a human-readable label.
fn capability_label(cap: &CapabilityRequest) -> String {
    match cap {
        CapabilityRequest::FilesystemRead { globs } => {
            if globs.is_empty() {
                "Read files (all)".to_string()
            } else {
                format!("Read files (globs: {})", globs.join(", "))
            }
        }
        CapabilityRequest::FilesystemWrite { globs } => {
            if globs.is_empty() {
                "Write files (all)".to_string()
            } else {
                format!("Write files (globs: {})", globs.join(", "))
            }
        }
        CapabilityRequest::NetworkOutbound { domains } => {
            if domains.is_empty() {
                "Network access (all domains)".to_string()
            } else {
                format!("Network access (domains: {})", domains.join(", "))
            }
        }
        CapabilityRequest::ShellExecute { allowlist } => {
            if allowlist.is_empty() {
                "Execute commands (all)".to_string()
            } else {
                format!("Execute commands (allowlist: {})", allowlist.join(", "))
            }
        }
        CapabilityRequest::Other { description } => description.clone(),
        _ => "unknown capability".to_string(),
    }
}

/// Read a single g/a/d decision from stdin for one capability.
fn prompt_single_capability(label: &str) -> Result<GrantDecision, PluginError> {
    eprintln!("  [{label}]");
    eprintln!("    [g]rant once, [a]lways allow, [d]eny");
    eprint!("    > ");
    io::stderr().flush().ok();

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|e| PluginError::ToolCallFailed(format!("failed to read input: {e}")))?;

    Ok(match input.trim().to_lowercase().as_str() {
        "g" => GrantDecision::Granted,
        "a" => GrantDecision::GrantedPersistent,
        _ => GrantDecision::Denied,
    })
}

#[async_trait::async_trait]
impl CapabilityApprovalUI for CliPluginApproval {
    async fn request(
        &self,
        plugin: &PluginManifest,
        capabilities: &[CapabilityRequest],
    ) -> Result<Vec<GrantDecision>, PluginError> {
        if !self.interactive {
            eprintln!(
                "[plugin] '{}' requires {} capabilities, denying (non-interactive mode)",
                plugin.name,
                capabilities.len()
            );
            return Ok(capabilities.iter().map(|_| GrantDecision::Denied).collect());
        }

        // Single capability → blanket prompt (fast path).
        if capabilities.len() == 1 {
            let label = capability_label(&capabilities[0]);
            eprintln!("\n=== Plugin Capability Request ===");
            eprintln!("Plugin: {} v{}", plugin.name, plugin.version);
            eprintln!("Description: {}", plugin.description);
            eprintln!("\nRequested capability:");
            eprintln!(" - {label}");

            let decision = prompt_single_capability(&label)?;
            return Ok(vec![decision]);
        }

        // Multiple capabilities → per-capability prompt.
        eprintln!("\n=== Plugin Capability Request ===");
        eprintln!("Plugin: {} v{}", plugin.name, plugin.version);
        eprintln!("Description: {}", plugin.description);
        eprintln!("\nThis plugin requests {} capabilities:", capabilities.len());
        for (i, cap) in capabilities.iter().enumerate() {
            eprintln!("  {}) {}", i + 1, capability_label(cap));
        }
        eprintln!("\nYou will be prompted for each capability individually.");
        eprintln!("Type 'a' to grant all remaining persistently, 'd' to deny this one only.\n");

        let mut decisions = Vec::with_capacity(capabilities.len());
        let mut blanket: Option<GrantDecision> = None;

        for (i, cap) in capabilities.iter().enumerate() {
            let label = capability_label(cap);

            // If a blanket decision was chosen earlier, apply it.
            if let Some(ref decision) = blanket {
                decisions.push(decision.clone());
                eprintln!("  [{label}] → {:?} (blanket)", decision);
                continue;
            }

            eprintln!("Capability {}/{}:", i + 1, capabilities.len());
            let decision = prompt_single_capability(&label)?;

            // If user chose 'a' (grant all) or 'd' (deny all), set the blanket.
            match &decision {
                GrantDecision::GrantedPersistent => {
                    blanket = Some(GrantDecision::GrantedPersistent);
                }
                GrantDecision::Denied => {
                    // Only set blanket deny if user types 'd' explicitly.
                    // 'd' already maps to Denied, but we don't blanket-deny
                    // unless they explicitly choose to (no blanket for denied).
                }
                _ => {}
            }

            decisions.push(decision);
        }

        Ok(decisions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_label_filesystem_read() {
        let cap = CapabilityRequest::FilesystemRead { globs: vec!["*.rs".into()] };
        assert_eq!(capability_label(&cap), "Read files (globs: *.rs)");
    }

    #[test]
    fn capability_label_filesystem_read_all() {
        let cap = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert_eq!(capability_label(&cap), "Read files (all)");
    }

    #[test]
    fn capability_label_shell_execute() {
        let cap = CapabilityRequest::ShellExecute { allowlist: vec!["git status".into()] };
        assert_eq!(capability_label(&cap), "Execute commands (allowlist: git status)");
    }

    #[test]
    fn capability_label_other() {
        let cap = CapabilityRequest::Other { description: "Custom capability".into() };
        assert_eq!(capability_label(&cap), "Custom capability");
    }
}
