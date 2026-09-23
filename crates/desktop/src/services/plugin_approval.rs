use crate::widgets::capability_dialog::{self, SharedPending};
use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
use concerto_plugins::capability::{CapabilityApprovalUI, GrantDecision};
use concerto_plugins::error::PluginError;

/// Desktop implementation of CapabilityApprovalUI.
///
/// Bridges the Iced capability dialog with the plugin approval flow. `Clone`
/// lets the Settings installer hand the service into a `Task::perform` while
/// the dialog keeps being driven from the App's shared pending queue.
#[derive(Clone)]
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
        // Create a coalescable channel for the decision.
        let (tx, rx) = tokio::sync::watch::channel(None);

        // Set the pending approval state.
        {
            let mut guard = self.pending.lock().map_err(|e| {
                PluginError::ToolCallFailed(format!("failed to lock pending approval: {e}"))
            })?;
            let key = format!("plugin:{}", plugin.id);
            guard.push_back(capability_dialog::PendingApproval {
                plugin: plugin.clone(),
                capabilities: capabilities.to_vec(),
                key,
                sender: tx,
                receiver: rx.clone(),
            });
        }

        // Wait for the user's decision via the coalescable channel.
        capability_dialog::await_decision(rx)
            .await
            .ok_or_else(|| PluginError::ToolCallFailed("approval dialog closed".to_owned()))
    }
}
