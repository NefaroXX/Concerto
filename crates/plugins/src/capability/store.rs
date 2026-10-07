//! File-backed persisted capability grant store
//! (`plugin_cap_grants.json`), including legacy migration and
//! prune-on-load.

use std::collections::HashMap;
use std::sync::Mutex;

use super::types::{now_unix, CapabilityDiscriminant, CapabilityScope, GrantPruneReport};
use crate::error::PluginError;

/// Default TTL for persistent capability grants (30 days, in seconds).
const GRANT_TTL_SECS: u64 = 30 * 24 * 3600;

/// Persisted grant entry — stores the discriminant plus its scope parameters,
/// with expiry and manifest hash pinning (ADR-37).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedGrant {
    disc: String,
    #[serde(default)]
    globs: Vec<String>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    allowlist: Vec<String>,
    /// Unix timestamp (seconds since epoch) of grant creation.
    #[serde(default)]
    created_at: u64,
    /// Unix timestamp (seconds since epoch) of grant expiry.
    #[serde(default)]
    expires_at: u64,
    /// SHA-256 of the WASM binary at approval time. `None` means "hash
    /// not established" (e.g. migrated legacy grants).
    #[serde(default)]
    manifest_hash: Option<String>,
}

impl PersistedGrant {
    fn from_discriminant_and_scope(
        disc: &CapabilityDiscriminant,
        scope: &CapabilityScope,
        manifest_hash: Option<String>,
    ) -> Self {
        let now = now_unix();
        Self {
            disc: format!("{disc:?}"),
            globs: scope.globs.clone(),
            domains: scope.domains.clone(),
            allowlist: scope.allowlist.clone(),
            created_at: now,
            expires_at: now + GRANT_TTL_SECS,
            manifest_hash,
        }
    }

    fn to_discriminant_and_scope(&self) -> Option<(CapabilityDiscriminant, CapabilityScope, u64)> {
        let disc = match self.disc.as_str() {
            "FilesystemRead" => CapabilityDiscriminant::FilesystemRead,
            "FilesystemWrite" => CapabilityDiscriminant::FilesystemWrite,
            "NetworkOutbound" => CapabilityDiscriminant::NetworkOutbound,
            "ShellExecute" => CapabilityDiscriminant::ShellExecute,
            _ => return None,
        };
        // Check TTL
        if now_unix() > self.expires_at {
            tracing::info!(?disc, expires_at = self.expires_at, "grant expired, skipping");
            return None;
        }
        let scope = CapabilityScope {
            globs: self.globs.clone(),
            domains: self.domains.clone(),
            allowlist: self.allowlist.clone(),
        };
        // Carry `expires_at` into the in-memory model so TTL enforcement can
        // also happen per call during a long session (audit finding H3).
        Some((disc, scope, self.expires_at))
    }

    /// Returns `true` if `manifest_hash` is `Some` and differs from the
    /// provided `wasm_hash`.  `None` means "not pinned" — no mismatch.
    fn hash_mismatch(&self, wasm_hash: Option<&str>) -> bool {
        match (&self.manifest_hash, wasm_hash) {
            (Some(stored), Some(current)) => stored != current,
            (None, Some(_)) => true, // runtime activation requires a binary pin
            _ => false,              // inspection without a current binary hash
        }
    }
}

/// A simple file-backed store for persistent capability grants.
pub(super) struct CapGrantStore {
    grants: Mutex<HashMap<String, Vec<PersistedGrant>>>,
    pub(super) path: std::path::PathBuf,
}

impl CapGrantStore {
    pub(super) fn open(data_dir: &std::path::Path) -> Result<Self, PluginError> {
        let path = data_dir.join("plugin_cap_grants.json");
        let grants = if path.exists() {
            let json_str = std::fs::read_to_string(&path).map_err(PluginError::Io)?;
            Self::parse_grants(&json_str).unwrap_or_default()
        } else {
            HashMap::new()
        };
        Ok(Self { grants: Mutex::new(grants), path })
    }

    /// Parse grants from JSON, handling both the new format (with scope + TTL)
    /// and the legacy format (bare discriminant strings).
    ///
    /// Legacy grants get a 24-hour migration TTL per ADR-37 (Option A).
    fn parse_grants(json_str: &str) -> Option<HashMap<String, Vec<PersistedGrant>>> {
        // Try new format first (PersistedGrant with serde default fields).
        if let Ok(grants) = serde_json::from_str::<HashMap<String, Vec<PersistedGrant>>>(json_str) {
            return Some(grants);
        }
        // Fall back to legacy format (plain discriminant strings).
        let old: HashMap<String, Vec<String>> = serde_json::from_str(json_str).ok()?;
        let now = now_unix();
        // Legacy grants: created 29 days ago, expires in 1 day (ADR-37 Option A).
        let legacy_created_at = now.saturating_sub(29 * 24 * 3600);
        let legacy_expires_at = now + 24 * 3600;
        Some(
            old.into_iter()
                .map(|(plugin_id, discriminants)| {
                    let grants: Vec<PersistedGrant> = discriminants
                        .into_iter()
                        .filter_map(|s| {
                            // Validate discriminant is known (legacy compat).
                            let valid = matches!(
                                s.as_str(),
                                "FilesystemRead"
                                    | "FilesystemWrite"
                                    | "NetworkOutbound"
                                    | "ShellExecute"
                            );
                            if !valid {
                                return None;
                            }
                            Some(PersistedGrant {
                                disc: s,
                                globs: vec![],
                                domains: vec![],
                                allowlist: vec![],
                                created_at: legacy_created_at,
                                expires_at: legacy_expires_at,
                                manifest_hash: None,
                            })
                        })
                        .collect();
                    (plugin_id, grants)
                })
                .collect(),
        )
    }

    /// Load grants for a plugin, filtering out expired entries and those whose
    /// manifest hash (if pinned) does not match the current WASM binary.
    ///
    /// Pruned entries are persisted back to disk so subsequent loads avoid
    /// re-processing stale grants (ADR-37 prune-on-load).
    ///
    /// Each returned entry is `(discriminant, scope, expires_at)` — the expiry
    /// is included so the in-memory grant model can keep enforcing the TTL per
    /// call after load.
    pub(super) fn load_for_plugin(
        &self,
        plugin_id: &str,
        wasm_hash: Option<&str>,
    ) -> (Vec<(CapabilityDiscriminant, CapabilityScope, u64)>, GrantPruneReport) {
        let pruned;
        let mut report = GrantPruneReport::default();
        let grants = {
            // Recover from poison in an infallible context.
            let mut store = self.grants.lock().unwrap_or_else(|e| e.into_inner());
            let Some(values) = store.get(plugin_id) else {
                return (vec![], report);
            };
            let count_before = values.len();

            // Keep only grants that survive hash pinning and expiry/format
            // checks; everything else is dropped from the store so the disk
            // write below removes it (ADR-37 prune-on-load).
            let retained: Vec<PersistedGrant> = values
                .iter()
                .filter(|g| {
                    if g.hash_mismatch(wasm_hash) {
                        report.hash_mismatch += 1;
                        tracing::info!(
                            plugin_id,
                            disc = %g.disc,
                            "grant hash mismatch (binary changed since approval), \
                             pruning and re-prompting"
                        );
                        return false;
                    }
                    if g.to_discriminant_and_scope().is_none() {
                        report.expired += 1;
                        return false;
                    }
                    true
                })
                .cloned()
                .collect();
            pruned = retained.len() < count_before;
            if pruned {
                if retained.is_empty() {
                    store.remove(plugin_id);
                } else {
                    store.insert(plugin_id.to_string(), retained.clone());
                }
            }

            retained.iter().filter_map(|g| g.to_discriminant_and_scope()).collect::<Vec<_>>()
        };
        // Persist the pruned grants back to disk (lock is released above).
        if pruned {
            if let Err(e) = self.write_store() {
                tracing::warn!(plugin_id, error = %e, "failed to persist pruned grants");
            }
        }
        (grants, report)
    }

    /// Serialize the current grants map and persist it to disk.
    ///
    /// Acquires the grants lock, serialises to JSON, creates the parent
    /// directory if needed, and writes `plugin_cap_grants.json`.
    fn write_store(&self) -> Result<(), PluginError> {
        let store = self.grants.lock().map_err(|_| {
            PluginError::Core(concerto_core::error::CoreError::EventBus(
                "capability grants lock poisoned".into(),
            ))
        })?;
        let json = serde_json::to_string(&*store).map_err(|e| {
            PluginError::Core(concerto_core::error::CoreError::EventBus(e.to_string()))
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.path, json).map_err(PluginError::Io)?;
        Ok(())
    }

    /// Save a grant with an optional manifest hash.
    pub(super) fn save_grant(
        &self,
        plugin_id: &str,
        cap: &CapabilityDiscriminant,
        scope: &CapabilityScope,
        manifest_hash: Option<String>,
    ) -> Result<(), PluginError> {
        {
            let mut store = self.grants.lock().map_err(|_| {
                PluginError::Core(concerto_core::error::CoreError::EventBus(
                    "capability grants lock poisoned".into(),
                ))
            })?;
            let entry = store.entry(plugin_id.to_string()).or_default();
            let persisted = PersistedGrant::from_discriminant_and_scope(cap, scope, manifest_hash);
            // Avoid duplicates: replace an existing entry with the same
            // discriminant.
            if let Some(pos) = entry.iter().position(|g| g.disc == persisted.disc) {
                entry[pos] = persisted;
            } else {
                entry.push(persisted);
            }
        }
        self.write_store()
    }

    /// Revoke (delete) all grants for a plugin.
    pub(super) fn revoke_plugin(&self, plugin_id: &str) -> Result<(), PluginError> {
        let changed = {
            let mut store = self.grants.lock().map_err(|_| {
                PluginError::Core(concerto_core::error::CoreError::EventBus(
                    "capability grants lock poisoned".into(),
                ))
            })?;
            store.remove(plugin_id).is_some()
        };
        if changed {
            self.write_store()?;
            tracing::info!(plugin_id, "capability grants revoked");
        }
        Ok(())
    }

    /// List all plugins that have grants.
    pub(super) fn list_plugins(&self) -> Vec<String> {
        let store = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix();
        store
            .keys()
            .filter(|&id| {
                // Only include plugins with at least one non-expired grant.
                store
                    .get(id)
                    .map(|grants| grants.iter().any(|g| now <= g.expires_at))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }
}
