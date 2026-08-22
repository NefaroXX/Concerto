use crate::widgets::capability_dialog::{self, SharedPending};
use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
use concerto_plugins::capability::{CapabilityApprovalUI, GrantDecision};
use concerto_plugins::error::PluginError;

/// Desktop implementation of CapabilityApprovalUI.
///
/// Bridges the Iced capability dialog with the plugin approval flow.
pub struct PluginApprovalService {
    pending: SharedPending,
}

impl PluginApprovalService {
    pub fn new(pending: SharedPending) -> Self {
        Self { pending }
    }
}

#[async_trait::async_trait]
impl CapabilityApprovalUI for PluginApprovalService {
    async fn request(
        &self,
        plugin: &PluginManifest,
        capabilities: &[CapabilityRequest],
    ) -> Result<Vec<GrantDecision>, PluginError> {
        // Create a oneshot channel for the decision.
        let (tx, rx) = tokio::sync::oneshot::channel();

        // Set the pending approval state.
        {
            let mut guard = self.pending.lock().map_err(|e| {
                PluginError::ToolCallFailed(format!("failed to lock pending approval: {e}"))
            })?;
            guard.push_back(capability_dialog::PendingApproval {
                plugin: plugin.clone(),
                capabilities: capabilities.to_vec(),
                sender: tx,
            });
        }

        // Wait for the user's decision via the oneshot channel.
        rx.await.map_err(|e| PluginError::ToolCallFailed(format!("approval dialog closed: {e}")))
    }
}
