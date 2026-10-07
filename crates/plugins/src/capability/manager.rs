//! Capability approval orchestrator over the persisted grant store.

use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};

use crate::error::PluginError;

use super::store::CapGrantStore;
use super::types::{
    CapabilityApprovalUI, CapabilityDiscriminant, CapabilityScope, GrantDecision, GrantPruneReport,
};
/// Capability approval orchestrator.
pub struct CapabilityManager {
    grant_store: CapGrantStore,
}

impl CapabilityManager {
    pub fn open(data_dir: &std::path::Path) -> Result<Self, PluginError> {
        let grant_store = CapGrantStore::open(data_dir)?;
        Ok(Self { grant_store })
    }

    /// Reload persisted approvals before activation so another UI/process's revocation wins.
    pub fn reload(&mut self) -> Result<(), PluginError> {
        let directory = self
            .grant_store
            .path
            .parent()
            .ok_or_else(|| PluginError::InvalidManifest("invalid grant-store path".into()))?;
        self.grant_store = CapGrantStore::open(directory)?;
        Ok(())
    }

    /// The standard capability-store data directory (`<data_dir>/concerto/
    /// plugins`), shared by callers that need the store path without opening
    /// a manager first (e.g. the Settings revoke helper).
    pub fn data_dir() -> std::path::PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("concerto")
            .join("plugins")
    }

    /// Request capability approval from the user through the provided UI.
    ///
    /// Persists the full [`CapabilityRequest`] scope (domains, globs,
    /// allowlist) alongside the capability discriminant so that subsequent
    /// host-function checks can perform fine-grained enforcement.
    ///
    /// `manifest_hash` — an optional SHA-256 hex digest of the WASM binary —
    /// is stored alongside the grant for hash pinning (ADR-37).  When `None`,
    /// the grant is not pinned (legacy compatibility).
    pub async fn request_approval(
        &self,
        plugin: &PluginManifest,
        capabilities: &[CapabilityRequest],
        approval_ui: &dyn CapabilityApprovalUI,
        manifest_hash: Option<String>,
    ) -> Result<Vec<GrantDecision>, PluginError> {
        let decisions = approval_ui.request(plugin, capabilities).await?;
        if decisions.len() != capabilities.len() {
            return Err(PluginError::CapabilityDenied("incomplete approval response".into()));
        }

        for (i, decision) in decisions.iter().enumerate() {
            if let GrantDecision::GrantedPersistent = decision {
                if let Some(cap) = capabilities.get(i) {
                    let discriminant: CapabilityDiscriminant = cap.into();
                    let scope: CapabilityScope = cap.into();
                    self.grant_store.save_grant(
                        &plugin.id,
                        &discriminant,
                        &scope,
                        manifest_hash.clone(),
                    )?;
                }
            }
        }

        Ok(decisions)
    }

    /// Load persistent grants for a plugin, filtering out expired grants and
    /// those whose pinned hash does not match `wasm_hash`.
    ///
    /// Each returned entry is `(discriminant, scope, expires_at)`. The expiry
    /// is carried into the in-memory grant model so that TTL enforcement
    /// continues per call during a long session, not just at load time.
    ///
    /// Pass `wasm_hash` as `Some(hex)` when the current WASM binary hash is
    /// known (normal load path) or `None` to skip hash pinning checks (legacy
    /// compat / tests).
    pub fn load_grants(
        &self,
        plugin_id: &str,
        wasm_hash: Option<&str>,
    ) -> Vec<(CapabilityDiscriminant, CapabilityScope, u64)> {
        self.grant_store.load_for_plugin(plugin_id, wasm_hash).0
    }

    /// Like [`Self::load_grants`], additionally reporting how many persisted
    /// grants were pruned and why. Callers use the report to audit
    /// hash-mismatch / TTL prunes (ADR-37 prune-on-load).
    pub fn load_grants_with_report(
        &self,
        plugin_id: &str,
        wasm_hash: Option<&str>,
    ) -> (Vec<(CapabilityDiscriminant, CapabilityScope, u64)>, GrantPruneReport) {
        self.grant_store.load_for_plugin(plugin_id, wasm_hash)
    }

    /// Revoke (delete) all capability grants for a plugin.
    pub fn revoke_plugin(&self, plugin_id: &str) -> Result<(), PluginError> {
        self.grant_store.revoke_plugin(plugin_id)
    }

    /// List all plugin IDs that currently have non-expired grants.
    pub fn list_granted_plugins(&self) -> Vec<String> {
        self.grant_store.list_plugins()
    }

    /// Test-only: persist a grant with an explicit (possibly bogus) manifest
    /// hash, so prune-on-load paths can be exercised deterministically.
    #[cfg(test)]
    pub(crate) fn save_grant_for_test(
        &self,
        plugin_id: &str,
        cap: &CapabilityDiscriminant,
        scope: &CapabilityScope,
        manifest_hash: Option<String>,
    ) -> Result<(), PluginError> {
        self.grant_store.save_grant(plugin_id, cap, scope, manifest_hash)
    }
}

#[cfg(test)]
mod tests;
