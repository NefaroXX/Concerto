use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};

use crate::error::PluginError;

/// Compute SHA-256 hex digest of `data` for manifest hash pinning.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hex_encode(hasher.finalize().as_slice())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Discriminant for capability matching (coarse-grained).
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum CapabilityDiscriminant {
    FilesystemRead,
    FilesystemWrite,
    NetworkOutbound,
    ShellExecute,
    Other,
}

impl From<&CapabilityRequest> for CapabilityDiscriminant {
    fn from(req: &CapabilityRequest) -> Self {
        match req {
            CapabilityRequest::FilesystemRead { .. } => CapabilityDiscriminant::FilesystemRead,
            CapabilityRequest::FilesystemWrite { .. } => CapabilityDiscriminant::FilesystemWrite,
            CapabilityRequest::NetworkOutbound { .. } => CapabilityDiscriminant::NetworkOutbound,
            CapabilityRequest::ShellExecute { .. } => CapabilityDiscriminant::ShellExecute,
            CapabilityRequest::Other { .. } => CapabilityDiscriminant::Other,
            _ => CapabilityDiscriminant::Other,
        }
    }
}

/// The fine-grained scope parameters for a granted capability.
///
/// Each variant of [`CapabilityRequest`] carries domain-specific scope data
/// (globs for filesystem access, domains for network access, allowlist for
/// shell execution).  This struct stores the approved scope alongside the
/// coarse [`CapabilityDiscriminant`] so that host-function checks can
/// enforce the exact boundaries the user approved rather than allowing
/// blanket access to every path, URL, or command.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapabilityScope {
    /// File glob patterns (for `FilesystemRead`/`FilesystemWrite`).
    #[serde(default)]
    pub globs: Vec<String>,
    /// Network egress allowlist (for `NetworkOutbound`).
    ///
    /// Each entry is an egress rule of the form `[scheme://]host[:port]`:
    ///
    /// * `scheme` — omitted or `*` matches any scheme, otherwise an exact
    ///   (case-insensitive) match. A URL without an explicit scheme is
    ///   treated as `https` (same normalization the host extractor has
    ///   always applied).
    /// * `host` — exact match or parent-domain match: `example.com` covers
    ///   `example.com` plus any `*.example.com`. A leading `*.` is accepted
    ///   and means the same thing (`*.example.com` ≡ `example.com`).
    /// * `port` — omitted matches any port, otherwise an exact match. Use
    ///   the *effective* port (`url.port_or_known_default()`), so
    ///   `https://host` compares against `443`.
    ///
    /// Entries are **default-deny**: once this list is non-empty, every
    /// target that matches no entry is refused by `check_url_allowed`.
    /// An empty list means *no allowlist configured* and keeps the
    /// pre-existing fail-open behaviour (see [`check_url_allowed`]).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Allowed shell commands — exact match only (for `ShellExecute`).
    ///
    /// Each entry must be the full command string including arguments
    /// (e.g. `"git status"`, not `"git *"`).  Prefix/wildcard matching
    /// is intentionally not supported for shell commands because the
    /// command string is passed to `sh -c`, which interprets shell
    /// metacharacters — prefix matching would allow injection via
    /// chaining (`;`, `&&`, `||`, `|`, etc.).
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl From<&CapabilityRequest> for CapabilityScope {
    fn from(req: &CapabilityRequest) -> Self {
        match req {
            CapabilityRequest::FilesystemRead { globs } => {
                Self { globs: globs.clone(), ..Default::default() }
            }
            CapabilityRequest::FilesystemWrite { globs } => {
                Self { globs: globs.clone(), ..Default::default() }
            }
            CapabilityRequest::NetworkOutbound { domains } => {
                Self { domains: domains.clone(), ..Default::default() }
            }
            CapabilityRequest::ShellExecute { allowlist } => {
                Self { allowlist: allowlist.clone(), ..Default::default() }
            }
            CapabilityRequest::Other { .. } => Self::default(),
            _ => Self::default(),
        }
    }
}

/// A persistent capability grant held in memory for a live plugin.
///
/// Carries the `expires_at` timestamp so that TTL enforcement happens per
/// host-function call, not only at load time (audit finding H3). Entries are
/// created by [`GrantedCapabilities::with_persistent`] from the capability
/// store; the load-time TTL filter guarantees `expires_at` is in the future
/// on arrival, and per-call checks deny the grant once it lapses mid-session.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PersistentGrant {
    /// Approved scope for the capability.
    pub(crate) scope: CapabilityScope,
    /// Unix timestamp (seconds since epoch) at which this grant expires.
    pub(crate) expires_at: u64,
}

/// Tracks granted capabilities (with scope) for a single plugin session.
#[derive(Default, Clone)]
pub struct GrantedCapabilities {
    /// Capabilities granted for this session only (not persisted).
    ///
    /// Maps each discriminant to its approved scope.  An empty/default scope
    /// (all fields empty) means "unrestricted within this capability class" —
    /// matching the pre-scope behaviour.
    pub(crate) session_grants: HashMap<CapabilityDiscriminant, CapabilityScope>,
    /// Persistent grants keyed by plugin ID, each with its own expiry.
    pub(crate) persistent_grants: HashMap<String, HashMap<CapabilityDiscriminant, PersistentGrant>>,
    /// Optional root directory — all file I/O is confined to this tree.
    pub root_dir: Option<std::path::PathBuf>,
}

impl GrantedCapabilities {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore from previously persisted grants (loaded at startup).
    ///
    /// Each entry is `(discriminant, scope, expires_at)`; the expiry is carried
    /// into the in-memory model so that TTL enforcement happens per call, not
    /// just at load time. The load-time filter in the capability store already
    /// guarantees `expires_at` is in the future when this is invoked.
    pub fn with_persistent(
        plugin_id: &str,
        grants: Vec<(CapabilityDiscriminant, CapabilityScope, u64)>,
    ) -> Self {
        let mut persistent_grants: HashMap<
            String,
            HashMap<CapabilityDiscriminant, PersistentGrant>,
        > = HashMap::new();
        persistent_grants.insert(
            plugin_id.to_string(),
            grants
                .into_iter()
                .map(|(disc, scope, expires_at)| (disc, PersistentGrant { scope, expires_at }))
                .collect(),
        );
        Self { session_grants: HashMap::new(), persistent_grants, root_dir: None }
    }

    /// Set a root directory — all file paths must start with this prefix.
    pub fn set_root(&mut self, root: std::path::PathBuf) {
        self.root_dir = Some(root);
    }

    /// Check if a specific capability discriminant is granted for a plugin.
    ///
    /// This is the coarse gate — it only checks *whether* the capability kind
    /// was approved.  Use [`get_scope`](Self::get_scope) for fine-grained
    /// enforcement of domains, globs, or allowlists.
    pub fn check(&self, plugin_id: &str, request: &CapabilityRequest) -> bool {
        let discriminant: CapabilityDiscriminant = request.into();
        if let Some(map) = self.persistent_grants.get(plugin_id) {
            if let Some(grant) = map.get(&discriminant) {
                // Expiry is enforced per call: a persistent grant that expires
                // mid-session is denied immediately, and a revoked/expired grant
                // can never be re-activated without a fresh approval. An expired
                // persistent grant falls through to the session check below — a
                // session grant is a separate, still-current authorization.
                if now_unix() <= grant.expires_at {
                    return true;
                }
            }
        }
        self.session_grants.contains_key(&discriminant)
    }

    /// Return the approved scope for a capability, if granted.
    ///
    /// Returns `None` when the capability is not granted at all.  Returns
    /// `Some(CapabilityScope::default())` when granted without restrictions
    /// (all fields empty = unrestricted access for that capability class).
    pub fn get_scope(
        &self,
        plugin_id: &str,
        discriminant: &CapabilityDiscriminant,
    ) -> Option<&CapabilityScope> {
        if let Some(map) = self.persistent_grants.get(plugin_id) {
            if let Some(grant) = map.get(discriminant) {
                // Same per-call TTL gate as `check`: an expired persistent grant
                // falls through to the session grants, which never expire
                // (session-scoped by definition, per ADR-37).
                if now_unix() <= grant.expires_at {
                    return Some(&grant.scope);
                }
            }
        }
        self.session_grants.get(discriminant)
    }

    /// Grant a capability for the current session with the given scope.
    pub fn grant_session(&mut self, cap: CapabilityDiscriminant, scope: CapabilityScope) {
        self.session_grants.insert(cap, scope);
    }

    /// Persist a capability grant (survives restarts) with the given scope.
    ///
    /// Note: this in-memory grant carries NO expiry — it has session-scoped
    /// semantics (valid until the grant set is cleared or replaced). Grants
    /// that survive restarts with a TTL arrive via
    /// [`with_persistent`](Self::with_persistent) after being loaded from the
    /// capability store.
    pub fn persist(
        &mut self,
        plugin_id: &str,
        cap: CapabilityDiscriminant,
        scope: CapabilityScope,
    ) {
        self.persistent_grants.entry(plugin_id.to_string()).or_default().insert(
            cap,
            // `persist()` grants without expiry: `u64::MAX` is a sentinel that
            // never lapses (`now_unix()` can never exceed it).
            PersistentGrant { scope, expires_at: u64::MAX },
        );
    }
}

/// Outcome of a user's decision on a single capability.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GrantDecision {
    Granted,
    GrantedPersistent,
    Denied,
}

/// Abstraction over Iced dialog and CLI prompt for capability approval.
#[async_trait::async_trait]
pub trait CapabilityApprovalUI: Send + Sync {
    async fn request(
        &self,
        plugin: &PluginManifest,
        capabilities: &[CapabilityRequest],
    ) -> Result<Vec<GrantDecision>, PluginError>;
}

/// Runtime discovery never authorizes a new capability request.
pub struct DenyUnapproved;
#[async_trait::async_trait]
impl CapabilityApprovalUI for DenyUnapproved {
    async fn request(
        &self,
        _: &PluginManifest,
        capabilities: &[CapabilityRequest],
    ) -> Result<Vec<GrantDecision>, PluginError> {
        Ok(vec![GrantDecision::Denied; capabilities.len()])
    }
}

/// Default TTL for persistent capability grants (30 days, in seconds).
const GRANT_TTL_SECS: u64 = 30 * 24 * 3600;

/// Returns the current Unix timestamp in seconds.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How many persisted grants were pruned during one load and why (ADR-37
/// prune-on-load). Callers audit a non-empty report so grant loss is visible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrantPruneReport {
    /// Grants dropped because the pinned WASM hash no longer matches.
    pub hash_mismatch: usize,
    /// Grants dropped because their TTL elapsed or their format was invalid.
    pub expired: usize,
}

impl GrantPruneReport {
    /// Total grants pruned.
    pub fn total(&self) -> usize {
        self.hash_mismatch + self.expired
    }

    /// Whether any grant was pruned.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

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
struct CapGrantStore {
    grants: Mutex<HashMap<String, Vec<PersistedGrant>>>,
    path: std::path::PathBuf,
}

impl CapGrantStore {
    fn open(data_dir: &std::path::Path) -> Result<Self, PluginError> {
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
    fn load_for_plugin(
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
    fn save_grant(
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
    fn revoke_plugin(&self, plugin_id: &str) -> Result<(), PluginError> {
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
    fn list_plugins(&self) -> Vec<String> {
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

// --- Capability checking helpers for host functions ---

/// Check whether a file path operation is permitted by the granted capabilities.
///
/// `is_write` must be `true` for write operations (FilesystemWrite capability),
/// `false` for read-only operations (FilesystemRead capability).
///
/// Rejects paths that are not absolute, or fall outside the configured root
/// directory scope.  For existing paths the target is canonicalized; for
/// non-existing paths (common during writes) the parent directory is
/// canonicalized instead to avoid spurious `ENOENT` from `canonicalize`.
///
/// A missing root directory fails closed: an unconfined capability cannot
/// confine any path, so it denies rather than skipping root and glob checks
/// (defect W7-1). Callers receive the *resolved* canonical path so they can
/// forward exactly what was validated to the executor, closing the
/// validate-then-use race (defect W7-2).
///
/// When the capability was granted with explicit glob patterns, the path
/// (relative to root) must match at least one of them.  An empty globs list
/// means unrestricted within the root directory (backwards-compatible).
pub fn check_path_allowed(
    caps: &GrantedCapabilities,
    plugin_id: &str,
    path: &str,
    is_write: bool,
) -> Result<std::path::PathBuf, PluginError> {
    // 1. Capability check — caller states read vs write explicitly.
    let discriminant = if is_write {
        CapabilityDiscriminant::FilesystemWrite
    } else {
        CapabilityDiscriminant::FilesystemRead
    };
    let cap_request = if is_write {
        CapabilityRequest::FilesystemWrite { globs: vec![] }
    } else {
        CapabilityRequest::FilesystemRead { globs: vec![] }
    };
    if !caps.check(plugin_id, &cap_request) {
        let label = if is_write { "FilesystemWrite" } else { "FilesystemRead" };
        return Err(PluginError::CapabilityDenied(label.into()));
    }

    // 2. Path must be absolute.
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err(PluginError::CapabilityDenied(
            "FilesystemRead/Write: only absolute paths are allowed".into(),
        ));
    }

    // 3. A root directory is mandatory. A `GrantedCapabilities` value that
    //    never had `set_root` called (e.g. `GrantedCapabilities::new()`) has
    //    no confinement boundary, so deny instead of silently skipping the
    //    root and glob checks below (fail-closed).
    let root = caps
        .root_dir
        .as_ref()
        .ok_or_else(|| PluginError::CapabilityDenied("no root directory configured".into()))?;

    // 4. Resolve the target path for root-scoped comparison.
    //
    //    `canonicalize` fails for non-existent files (common on write), so
    //    we canonicalize parent + append filename as a fallback.
    let resolved = resolve_for_comparison(p, is_write)?;

    // 5. Root-scoped access: target must live under root_dir.
    let root_path = std::fs::canonicalize(root)
        .map_err(|_| PluginError::CapabilityDenied("invalid root directory".into()))?;
    if !resolved.starts_with(&root_path) {
        return Err(PluginError::CapabilityDenied(format!(
            "FilesystemRead/Write: path {} is outside root directory {}",
            path,
            root.display(),
        )));
    }

    // 6. Glob scope check: if the grant has non-empty globs, the
    //    relative path under root must match at least one pattern.
    if let Some(scope) = caps.get_scope(plugin_id, &discriminant) {
        if !scope.globs.is_empty() {
            let relative = resolved.strip_prefix(&root_path).unwrap_or(&resolved);
            let matched = scope.globs.iter().any(|g| glob_match(g, relative));
            if !matched {
                return Err(PluginError::CapabilityDenied(format!(
                    "Filesystem{}: path '{}' does not match any allowed glob pattern {:?}",
                    if is_write { "Write" } else { "Read" },
                    relative.display(),
                    scope.globs,
                )));
            }
        }
    }
    Ok(resolved)
}

/// Resolve `p` to a real path for root-scoped comparison.
///
/// If the path exists, canonicalize it directly.  Otherwise fall back to
/// canonicalizing the parent directory and appending the file name (this
/// handles the common write-to-new-file case without failing on `ENOENT`).
fn resolve_for_comparison(
    p: &std::path::Path,
    is_write: bool,
) -> Result<std::path::PathBuf, PluginError> {
    if p.exists() {
        return std::fs::canonicalize(p)
            .map_err(|e| PluginError::CapabilityDenied(format!("canonicalize failed: {e}")));
    }
    // Non-existent path — resolve parent instead.
    let parent =
        p.parent().ok_or_else(|| PluginError::CapabilityDenied("path has no parent".into()))?;
    // Reject parent-dir traversal in the unresolvable portion.
    if parent.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(PluginError::CapabilityDenied("path traversal (..) is not allowed".into()));
    }
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|e| PluginError::CapabilityDenied(format!("invalid parent directory: {e}")))?;
    // For writes the file name must be present; for reads it's an error.
    let file_name = p.file_name().ok_or_else(|| {
        if is_write {
            PluginError::CapabilityDenied("invalid file name".into())
        } else {
            PluginError::CapabilityDenied("read path does not exist and has no file name".into())
        }
    })?;
    Ok(canonical_parent.join(file_name))
}

/// Simple glob pattern matching for path components.
///
/// Supports:
/// - `*` — matches any characters except `/`
/// - `**` — matches any characters including `/`
/// - `?` — matches any single character except `/`
///
/// A pattern without any wildcards is treated as an exact (byte-for-byte) match.
fn glob_match(pattern: &str, path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    // Fast path: no wildcards → exact string comparison.
    if !pattern.contains('*') && !pattern.contains('?') {
        return path_str.as_ref() == pattern;
    }

    let pat_bytes = pattern.as_bytes();
    let path_bytes = path_str.as_bytes();

    glob_match_bytes(pat_bytes, path_bytes, false)
}

/// Recursive byte-level glob matcher.
///
/// `consumed_slash` tracks whether the previous pattern segment ended with `**`
/// matching a `/`, which prevents `**` from matching zero path segments more
/// than once (avoids exponential blowup on `**/**/**...` patterns).
fn glob_match_bytes(pattern: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    if pattern.is_empty() {
        return input.is_empty();
    }

    // `**` — match any number of characters (including path separators).
    if is_double_star(pattern) {
        return match_double_star(pattern, input, consumed_slash);
    }

    if input.is_empty() {
        return false;
    }

    match pattern[0] {
        b'*' => match_single_star(&pattern[1..], input, consumed_slash),
        b'?' => match_qmark(&pattern[1..], input),
        _ => match_literal(pattern, input),
    }
}

/// Check whether the pattern starts with `**`.
fn is_double_star(pattern: &[u8]) -> bool {
    pattern.len() >= 2 && &pattern[..2] == b"**"
}

/// Match `**/` — zero or more directory levels.
///
/// `rest` is the pattern after `**` (starts with `/`).  Tries zero
/// directory levels first (skip `**/` entirely), then one or more
/// levels (consume characters including at least one `/`).
fn match_double_star_slash(rest: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    debug_assert!(rest.starts_with(b"/"), "expected rest to start with '/'");

    let after = &rest[1..];

    // Try zero directory levels: skip `**/` entirely.
    if glob_match_bytes(after, input, consumed_slash) {
        return true;
    }
    // Try one or more directory levels: consume at least one character,
    // matching through / past at least one `/`.
    for i in 1..=input.len() {
        if glob_match_bytes(rest, &input[i..], false) {
            return true;
        }
    }
    false
}

/// Match bare `**` (no following `/`).
///
/// If `rest` is non-empty, tries matching zero characters (skip `**`
/// entirely) then one or more characters.  A trailing `**` with no
/// following pattern matches everything.
fn match_bare_double_star(rest: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    if !rest.is_empty() {
        // Try matching zero characters (skip `**` entirely).
        if glob_match_bytes(rest, input, consumed_slash) {
            return true;
        }
        // Try matching one or more characters.
        for i in 1..=input.len() {
            if glob_match_bytes(rest, &input[i..], false) {
                return true;
            }
        }
        return false;
    }

    // Trailing `**` matches everything.
    true
}

/// Match `**/` (zero or more directory levels) or bare `**`.
fn match_double_star(pattern: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    let rest = &pattern[2..];

    if rest.starts_with(b"/") {
        return match_double_star_slash(rest, input, consumed_slash);
    }

    match_bare_double_star(rest, input, consumed_slash)
}

/// Match `*` that can cross `/` (after `**` already crossed a separator).
fn match_star_crossing(rest: &[u8], input: &[u8]) -> bool {
    for i in 0..=input.len() {
        if glob_match_bytes(rest, &input[i..], false) {
            return true;
        }
    }
    false
}

/// Match `*` — any characters except `/`.
fn match_single_star(rest: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    if consumed_slash {
        // `**` already crossed a separator, so this `*` can cross too.
        return match_star_crossing(rest, input);
    }
    // Normal `*`: match any non-`/` characters.
    for i in 0..=input.len() {
        if i > 0 && input[i - 1] == b'/' {
            break;
        }
        if glob_match_bytes(rest, &input[i..], false) {
            return true;
        }
    }
    false
}

/// Match `?` — exactly one character except `/`.
fn match_qmark(rest: &[u8], input: &[u8]) -> bool {
    if input.is_empty() || input[0] == b'/' {
        return false;
    }
    glob_match_bytes(rest, &input[1..], false)
}

/// Match a literal character.
fn match_literal(pattern: &[u8], input: &[u8]) -> bool {
    if input.is_empty() || input[0] != pattern[0] {
        return false;
    }
    glob_match_bytes(&pattern[1..], &input[1..], false)
}

/// Rule name recorded in every network-egress denial raised by
/// [`check_url_allowed`] when the configured allowlist refuses a target.
///
/// Snake-case, mirroring the policy engine's own rule identifiers
/// (`deny_network_egress`, `auto_deny`, …) so audit rows and error strings
/// can be correlated across subsystems (threat model §6 gap #7).
pub const RULE_EGRESS_ALLOWLIST: &str = "egress_allowlist";

/// Rule name recorded when the coarse `NetworkOutbound` capability was never
/// granted at all — the gate that runs *before* the allowlist.
pub const RULE_NETWORK_CAPABILITY: &str = "network_capability";

/// Scheme / host / port triple parsed out of a target URL so an egress rule
/// can be matched against every dimension it constrains.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EgressTarget {
    /// Lowercased URL scheme (no trailing `:`).
    scheme: String,
    /// Lowercased host, IPv6 brackets stripped (`::1`, not `[::1]`).
    host: String,
    /// Effective port — `port_or_known_default()`, so `https://x` yields `443`.
    /// `None` only for a non-special scheme with no explicit port.
    port: Option<u16>,
}

/// One parsed egress allowlist entry: `[scheme://]host[:port]`.
///
/// Parsing is deliberately strict and *fail-closed*: an entry that cannot be
/// interpreted (empty host, non-numeric port, bare `*`) yields `None` and is
/// never treated as a match. A malformed rule can therefore only narrow what
/// is reachable, never widen it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EgressRule {
    /// `None` = any scheme (omitted or `*`).
    scheme: Option<String>,
    /// Exact or parent-domain pattern, lowercased.
    host: String,
    /// `None` = any port.
    port: Option<u16>,
}

impl EgressRule {
    /// Parse a single allowlist entry. Returns `None` when the entry can
    /// never match a well-formed target (see the type-level docs).
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        let (scheme, rest) = match entry.split_once("://") {
            Some((scheme, rest)) => {
                let scheme = scheme.trim().to_ascii_lowercase();
                if scheme.is_empty() {
                    return None;
                }
                ((scheme != "*").then_some(scheme), rest.trim())
            }
            None => (None, entry),
        };
        let (host, port) = split_rule_host_port(rest)?;
        let host = host.to_ascii_lowercase();
        // `*` and `*.` are not host patterns — reject them outright so a
        // wildcard-escape can never read as "allow every host".
        let bare = host.strip_prefix("*.").unwrap_or(&host);
        if bare.is_empty() || bare == "*" {
            return None;
        }
        Some(Self { scheme, host, port })
    }

    /// Whether this rule admits `target`. Every dimension the rule names
    /// must match; an unnamed dimension is unconstrained.
    fn matches(&self, target: &EgressTarget) -> bool {
        if let Some(scheme) = &self.scheme {
            if *scheme != target.scheme {
                return false;
            }
        }
        if let Some(port) = self.port {
            if target.port != Some(port) {
                return false;
            }
        }
        host_pattern_matches(&self.host, &target.host)
    }
}

/// Split `[scheme://]host[:port]`'s host/port part.
///
/// Handles three shapes: bracketed IPv6 (`[::1]:8443`), a single `:` that
/// must be a numeric port, and everything else treated as a bare host (which
/// also covers an unbracketed IPv6 literal such as `::1`).
fn split_rule_host_port(rest: &str) -> Option<(&str, Option<u16>)> {
    if rest.is_empty() {
        return None;
    }
    if let Some(inner) = rest.strip_prefix('[') {
        let close = inner.find(']')?;
        let host = &inner[..close];
        let tail = &inner[close + 1..];
        let port = match tail {
            "" => None,
            _ => Some(tail.strip_prefix(':')?.parse::<u16>().ok()?),
        };
        return (!host.is_empty()).then_some((host, port));
    }
    match rest.matches(':').count() {
        0 => Some((rest, None)),
        1 => {
            let (host, port) = rest.split_once(':')?;
            // A non-numeric or out-of-range port makes the whole entry
            // unusable rather than silently dropping the port constraint.
            let port = port.parse::<u16>().ok()?;
            (!host.is_empty()).then_some((host, Some(port)))
        }
        // Two or more colons and no brackets → the literal is an IPv6
        // address with no port component.
        _ => Some((rest, None)),
    }
}

/// Exact or parent-domain match: `example.com` covers `example.com` and any
/// `sub.example.com`. A leading `*.` is normalized away (it used to be a
/// dead entry that matched nothing).
fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let base = pattern.strip_prefix("*.").unwrap_or(pattern);
    if base.is_empty() {
        return false;
    }
    host == base || host.ends_with(&format!(".{base}"))
}

/// Split `url` into its scheme/host/port triple for egress matching.
///
/// Keeps the normalization this check has always applied: trim, treat a
/// scheme-less `host/path` form as `https`, and lowercase the host.
fn extract_egress_target(url: &str) -> Result<EgressTarget, PluginError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(PluginError::CapabilityDenied("URL has no host: (empty)".into()));
    }

    // If no scheme is present, url::Url::parse requires one — prepend
    // a dummy scheme so bare `host/path` forms parse correctly.
    let url_to_parse = if url.contains("://") { url.to_string() } else { format!("https://{url}") };

    let parsed = url::Url::parse(&url_to_parse)
        .map_err(|e| PluginError::CapabilityDenied(format!("invalid URL: {e}")))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| PluginError::CapabilityDenied(format!("URL has no host: {url}")))?
        .to_lowercase();
    // `host_str()` serializes IPv6 with brackets; strip them so targets and
    // rule entries share one canonical host form.
    let host =
        host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&host).to_string();

    Ok(EgressTarget {
        scheme: parsed.scheme().to_ascii_lowercase(),
        host,
        port: parsed.port_or_known_default(),
    })
}

/// Extract the hostname part from a URL string.
///
/// Returns the lowercased host portion, or an error if the URL cannot
/// be parsed or has no host. Test-only convenience over
/// [`extract_egress_target`] (the production path needs scheme/port too).
#[cfg(test)]
fn extract_url_host(url: &str) -> Result<String, PluginError> {
    extract_egress_target(url).map(|target| target.host)
}

/// Evaluate the configured egress allowlist against `target`.
///
/// Default-deny: at least one entry must admit the target. Entries that fail
/// to parse are warned about once per check and never count as a match.
fn match_egress_allowlist(
    plugin_id: &str,
    allowlist: &[String],
    target: &EgressTarget,
    url: &str,
) -> Result<(), PluginError> {
    let mut invalid: Vec<&str> = Vec::new();
    for entry in allowlist {
        match EgressRule::parse(entry) {
            Some(rule) if rule.matches(target) => return Ok(()),
            Some(_) => {}
            None => invalid.push(entry.as_str()),
        }
    }
    if !invalid.is_empty() {
        tracing::warn!(
            plugin_id,
            ?invalid,
            "invalid network egress allowlist entries never match; they grant nothing"
        );
    }
    Err(PluginError::CapabilityDenied(format!(
        "{RULE_EGRESS_ALLOWLIST}: network egress to '{url}' denied \
         (scheme={} host={} port={:?}); allowlist: {allowlist:?}",
        target.scheme, target.host, target.port,
    )))
}

/// Check whether a URL is permitted by the granted capabilities.
///
/// Enforcement steps:
/// 1. Reject if the `NetworkOutbound` capability was never granted at all
///    (rule [`RULE_NETWORK_CAPABILITY`]).
/// 2. If the grant carries an **empty** allowlist, no egress rules were
///    configured for this plugin. **Documented choice: fail open with a
///    warning.** Blanket `NetworkOutbound` grants (an empty `domains` list)
///    are the pre-existing, user-approved behaviour, and silently turning
///    them into a deny would break every already-approved plugin; a
///    `tracing::warn!` records each unrestricted attempt so operators can
///    find and tighten those grants. Configure at least one entry to get
///    threat-model §6 gap #7 enforcement.
/// 3. Otherwise the allowlist is **default-deny**: parse the target's
///    scheme/host/port and require a rule match (rule
///    [`RULE_EGRESS_ALLOWLIST`]). The returned error names the rule and the
///    offending target so it can be matched by callers and audit rows.
pub fn check_url_allowed(
    caps: &GrantedCapabilities,
    plugin_id: &str,
    url: &str,
) -> Result<(), PluginError> {
    let request = CapabilityRequest::NetworkOutbound { domains: vec![] };
    if !caps.check(plugin_id, &request) {
        return Err(PluginError::CapabilityDenied(format!(
            "{RULE_NETWORK_CAPABILITY}: the NetworkOutbound capability is not granted",
        )));
    }

    let Some(scope) = caps.get_scope(plugin_id, &CapabilityDiscriminant::NetworkOutbound) else {
        // `check()` and `get_scope()` walk the same grant sets, so this is
        // unreachable; fail open with a warning rather than inventing a
        // denial the user never configured.
        tracing::warn!(plugin_id, "NetworkOutbound granted without a scope; failing open");
        return Ok(());
    };

    if scope.domains.is_empty() {
        tracing::warn!(
            plugin_id,
            "plugin network egress is unrestricted: no allowlist configured for the \
             NetworkOutbound grant; failing open"
        );
        return Ok(());
    }

    // Parse only after the unconfigured fast path so an unconfigured grant
    // keeps accepting exactly what it accepted before (including URLs that
    // do not parse — reqwest rejects those later anyway).
    let target = extract_egress_target(url)?;
    match_egress_allowlist(plugin_id, &scope.domains, &target, url)
}

/// Check whether a shell command is permitted by the granted capabilities.
///
/// Enforcement steps:
/// 1. Reject if the `ShellExecute` capability was never granted at all.
/// 2. If the capability was granted with a non-empty `allowlist`, verify that
///    the command matches at least one entry **exactly** (byte-for-byte).
///    An empty allowlist means "all commands allowed" (backwards-compatible
///    behaviour).
///
/// # Security: why only exact matches?
///
/// Earlier versions supported `*`-suffixed prefix patterns (e.g. `git *`
/// matching `git status`).  This was unsafe because the command string is
/// passed to `sh -c` on the host, which interprets shell metacharacters.
/// A plugin scoped to `["git *"]` could send `git status; rm -rf /` — the
/// prefix check passes, but `sh -c` runs both commands.
///
/// Exact-match enforcement means the allowlist entries must contain the full
/// command including arguments (e.g. `"git status"`, not `"git *"`).
/// This eliminates the injection vector entirely.
pub fn check_shell_allowed(
    caps: &GrantedCapabilities,
    plugin_id: &str,
    command: &str,
) -> Result<(), PluginError> {
    let request = CapabilityRequest::ShellExecute { allowlist: vec![] };
    if !caps.check(plugin_id, &request) {
        return Err(PluginError::CapabilityDenied("ShellExecute".into()));
    }

    // Fine-grained allowlist check — exact match only.
    if let Some(scope) = caps.get_scope(plugin_id, &CapabilityDiscriminant::ShellExecute) {
        if !scope.allowlist.is_empty() {
            let allowed = scope.allowlist.iter().any(|pattern| command == pattern);
            if !allowed {
                return Err(PluginError::CapabilityDenied(format!(
                    "ShellExecute: command '{command}' not in allowlist {:?}",
                    scope.allowlist,
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_cap_check_returns_false_by_default() {
        let caps = GrantedCapabilities::new();
        let req = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert!(!caps.check("test-plugin", &req));
    }

    #[test]
    fn session_grant_is_checked() {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
        let req = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert!(caps.check("test-plugin", &req));
    }

    #[test]
    fn persistent_grant_survives_recreation() {
        let mut caps = GrantedCapabilities::new();
        caps.persist(
            "test-plugin",
            CapabilityDiscriminant::NetworkOutbound,
            CapabilityScope::default(),
        );
        let req = CapabilityRequest::NetworkOutbound { domains: vec![] };
        assert!(caps.check("test-plugin", &req));
    }

    #[test]
    fn ungranted_cap_is_denied() {
        let caps = GrantedCapabilities::new();
        let req = CapabilityRequest::ShellExecute { allowlist: vec![] };
        assert!(!caps.check("test-plugin", &req));
    }

    #[test]
    fn loaded_grants_are_checked() {
        let caps = GrantedCapabilities::with_persistent(
            "test-plugin",
            vec![(
                CapabilityDiscriminant::FilesystemRead,
                CapabilityScope::default(),
                now_unix() + 3600,
            )],
        );
        let req = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert!(caps.check("test-plugin", &req));
        let req2 = CapabilityRequest::NetworkOutbound { domains: vec![] };
        assert!(!caps.check("test-plugin", &req2));
    }

    // --- get_scope ---

    #[test]
    fn get_scope_returns_none_for_ungranted() {
        let caps = GrantedCapabilities::new();
        assert!(caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_none());
    }

    #[test]
    fn get_scope_returns_session_scope() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { globs: vec!["src/**/*.rs".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, scope.clone());
        let got = caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead);
        assert_eq!(got, Some(&scope));
    }

    #[test]
    fn get_scope_returns_persistent_scope() {
        let mut caps = GrantedCapabilities::new();
        let scope =
            CapabilityScope { domains: vec!["api.example.com".into()], ..Default::default() };
        caps.persist("p", CapabilityDiscriminant::NetworkOutbound, scope.clone());
        let got = caps.get_scope("p", &CapabilityDiscriminant::NetworkOutbound);
        assert_eq!(got, Some(&scope));
    }

    // --- per-call TTL enforcement (audit finding H3) ---

    #[test]
    fn expired_persistent_grant_denied_at_call_time() {
        let caps = GrantedCapabilities::with_persistent(
            "p",
            vec![(
                CapabilityDiscriminant::FilesystemRead,
                CapabilityScope::default(),
                now_unix() - 10,
            )],
        );
        let req = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert!(
            !caps.check("p", &req),
            "persistent grant past its TTL must be denied at call time"
        );
        assert!(
            caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_none(),
            "expired persistent grant must not yield a scope"
        );
    }

    #[test]
    fn unexpired_persistent_grant_allowed_at_call_time() {
        let caps = GrantedCapabilities::with_persistent(
            "p",
            vec![(
                CapabilityDiscriminant::FilesystemRead,
                CapabilityScope::default(),
                now_unix() + 3600,
            )],
        );
        let req = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert!(caps.check("p", &req), "unexpired persistent grant must be allowed");
        assert!(
            caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_some(),
            "unexpired persistent grant must yield a scope"
        );
    }

    #[test]
    fn expired_persistent_grant_falls_through_to_session_grant() {
        let mut caps = GrantedCapabilities::with_persistent(
            "p",
            vec![(
                CapabilityDiscriminant::FilesystemRead,
                CapabilityScope::default(),
                now_unix() - 10,
            )],
        );
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
        let req = CapabilityRequest::FilesystemRead { globs: vec![] };
        assert!(
            caps.check("p", &req),
            "a still-current session grant must authorize despite the expired persistent grant"
        );
        assert!(
            caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_some(),
            "session scope must still be returned when the persistent grant expired"
        );
    }

    // --- check_url_allowed ---

    #[test]
    fn url_allowed_when_no_domain_restrictions() {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(CapabilityDiscriminant::NetworkOutbound, CapabilityScope::default());
        assert!(check_url_allowed(&caps, "p", "https://evil.com/malware").is_ok());
    }

    #[test]
    fn url_denied_when_domain_not_in_allowlist() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
        let result = check_url_allowed(&caps, "p", "https://evil.com/malware");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("evil.com"));
    }

    #[test]
    fn url_allowed_when_domain_in_allowlist() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
        assert!(check_url_allowed(&caps, "p", "https://example.com/api").is_ok());
    }

    #[test]
    fn url_subdomain_matches_parent_domain() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
        // api.example.com is a subdomain of example.com → allowed.
        assert!(check_url_allowed(&caps, "p", "https://api.example.com/v1").is_ok());
    }

    #[test]
    fn url_not_granted_is_denied() {
        let caps = GrantedCapabilities::new();
        let result = check_url_allowed(&caps, "p", "https://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn url_rejects_invalid_url() {
        let mut caps = GrantedCapabilities::new();
        // Grant with a non-empty domain list so that URL parsing is exercised.
        let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
        let result = check_url_allowed(&caps, "p", "");
        assert!(result.is_err());
    }

    // --- check_shell_allowed ---

    #[test]
    fn shell_denied_when_not_granted() {
        let caps = GrantedCapabilities::new();
        let result = check_shell_allowed(&caps, "p", "echo hello");
        assert!(result.is_err());
    }

    #[test]
    fn shell_allowed_when_no_allowlist() {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(CapabilityDiscriminant::ShellExecute, CapabilityScope::default());
        assert!(check_shell_allowed(&caps, "p", "echo hello").is_ok());
    }

    #[test]
    fn shell_denied_when_not_in_allowlist() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { allowlist: vec!["git *".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
        let result = check_shell_allowed(&caps, "p", "rm -rf /");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not in allowlist"));
    }

    #[test]
    fn shell_allowed_when_exact_match_in_allowlist() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { allowlist: vec!["echo hello".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
        assert!(check_shell_allowed(&caps, "p", "echo hello").is_ok());
    }

    #[test]
    fn shell_prefix_match_rejected_exact_only() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { allowlist: vec!["git status".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
        // Exact match — allowed.
        assert!(check_shell_allowed(&caps, "p", "git status").is_ok());
        // Prefix only — rejected (no more `*` suffix matching).
        let result = check_shell_allowed(&caps, "p", "git commit -m 'fix'");
        assert!(result.is_err());
    }

    #[test]
    fn shell_injection_via_semicolon_rejected() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { allowlist: vec!["git status".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
        // Shell injection attempt — semicolon chains a second command.
        let result = check_shell_allowed(&caps, "p", "git status; rm -rf /");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not in allowlist"));
    }

    #[test]
    fn shell_injection_via_ampersand_rejected() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { allowlist: vec!["ls".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
        // Double-amperstand chains commands.
        let result = check_shell_allowed(&caps, "p", "ls && cat /etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn shell_injection_via_pipe_rejected() {
        let mut caps = GrantedCapabilities::new();
        let scope = CapabilityScope { allowlist: vec!["echo hello".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
        // Pipe chains commands.
        let result = check_shell_allowed(&caps, "p", "echo hello | sh");
        assert!(result.is_err());
    }

    // --- check_path_allowed (glob enforcement) ---
    //
    // Note: these tests create real temp directories because
    // `resolve_for_comparison` calls `canonicalize` and needs
    // the path to exist on disk.

    #[test]
    fn path_allowed_when_no_glob_restrictions() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file_path = root.join("src/main.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "").unwrap();

        let mut caps = GrantedCapabilities::new();
        caps.set_root(root);
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
        assert!(check_path_allowed(&caps, "p", file_path.to_str().unwrap(), false).is_ok());
    }

    #[test]
    fn path_denied_when_glob_does_not_match() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file_path = root.join("Makefile");
        std::fs::write(&file_path, "").unwrap();

        let mut caps = GrantedCapabilities::new();
        caps.set_root(root);
        let scope = CapabilityScope { globs: vec!["src/**/*.rs".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, scope);
        let result = check_path_allowed(&caps, "p", file_path.to_str().unwrap(), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not match"));
    }

    #[test]
    fn path_allowed_when_glob_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file_path = root.join("src/main.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "").unwrap();

        let mut caps = GrantedCapabilities::new();
        caps.set_root(root);
        let scope = CapabilityScope { globs: vec!["src/**/*.rs".into()], ..Default::default() };
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, scope);
        assert!(check_path_allowed(&caps, "p", file_path.to_str().unwrap(), false).is_ok());
    }

    // --- glob_match ---

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("src/main.rs", std::path::Path::new("src/main.rs")));
    }

    #[test]
    fn glob_wildcard_match() {
        assert!(glob_match("*.rs", std::path::Path::new("main.rs")));
    }

    #[test]
    fn glob_wildcard_no_match() {
        assert!(!glob_match("*.rs", std::path::Path::new("main.js")));
    }

    #[test]
    fn glob_double_star_match() {
        assert!(glob_match("src/**/*.rs", std::path::Path::new("src/a/b/c.rs")));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("?.rs", std::path::Path::new("a.rs")));
        assert!(!glob_match("?.rs", std::path::Path::new("ab.rs")));
    }

    // --- extract_url_host ---

    #[test]
    fn extract_https_host() {
        assert_eq!(extract_url_host("https://example.com/path").unwrap(), "example.com");
    }

    #[test]
    fn extract_host_with_port() {
        assert_eq!(extract_url_host("http://example.com:8080/path").unwrap(), "example.com");
    }

    #[test]
    fn extract_bare_host() {
        assert_eq!(extract_url_host("example.com").unwrap(), "example.com");
    }

    #[test]
    fn extract_bare_host_with_path() {
        assert_eq!(extract_url_host("example.com/api/v1").unwrap(), "example.com");
    }

    #[test]
    fn extract_host_error_on_empty() {
        assert!(extract_url_host("").is_err());
    }

    #[test]
    fn extract_host_with_userinfo_rejected() {
        // url::Url correctly parses userinfo — we should extract just the host.
        assert_eq!(extract_url_host("https://user:pass@example.com/path").unwrap(), "example.com");
    }

    #[test]
    fn extract_host_ip_address() {
        assert_eq!(extract_url_host("http://127.0.0.1:3000/api").unwrap(), "127.0.0.1");
    }

    #[test]
    fn extract_host_lowercase() {
        assert_eq!(extract_url_host("https://EXAMPLE.COM/path").unwrap(), "example.com");
    }

    // ------------------------------------------------------------------
    // CapabilityDiscriminant / CapabilityScope conversions
    // ------------------------------------------------------------------

    /// Every `CapabilityRequest` variant maps to the correct `CapabilityDiscriminant`.
    #[test]
    fn test_capability_discriminant_from_request_all_variants() {
        use concerto_api_types::plugin::CapabilityRequest;

        let cases: Vec<(CapabilityRequest, CapabilityDiscriminant)> = vec![
            (
                CapabilityRequest::FilesystemRead { globs: vec![] },
                CapabilityDiscriminant::FilesystemRead,
            ),
            (
                CapabilityRequest::FilesystemWrite { globs: vec![] },
                CapabilityDiscriminant::FilesystemWrite,
            ),
            (
                CapabilityRequest::NetworkOutbound { domains: vec![] },
                CapabilityDiscriminant::NetworkOutbound,
            ),
            (
                CapabilityRequest::ShellExecute { allowlist: vec![] },
                CapabilityDiscriminant::ShellExecute,
            ),
            (
                CapabilityRequest::Other { description: "custom".into() },
                CapabilityDiscriminant::Other,
            ),
        ];

        for (req, expected) in &cases {
            let disc: CapabilityDiscriminant = req.into();
            assert_eq!(
                disc, *expected,
                "mismatch for {req:?}: expected {expected:?}, got {disc:?}",
            );
        }
    }

    /// Every `CapabilityRequest` variant produces the correct `CapabilityScope`.
    #[test]
    fn test_capability_scope_from_request_all_variants() {
        use concerto_api_types::plugin::CapabilityRequest;

        let cases: Vec<(CapabilityRequest, CapabilityScope)> = vec![
            (
                CapabilityRequest::FilesystemRead { globs: vec!["*.rs".into()] },
                CapabilityScope { globs: vec!["*.rs".into()], ..Default::default() },
            ),
            (
                CapabilityRequest::FilesystemWrite { globs: vec!["/tmp/*".into()] },
                CapabilityScope { globs: vec!["/tmp/*".into()], ..Default::default() },
            ),
            (
                CapabilityRequest::NetworkOutbound { domains: vec!["example.com".into()] },
                CapabilityScope { domains: vec!["example.com".into()], ..Default::default() },
            ),
            (
                CapabilityRequest::ShellExecute { allowlist: vec!["git status".into()] },
                CapabilityScope { allowlist: vec!["git status".into()], ..Default::default() },
            ),
            (CapabilityRequest::Other { description: "custom".into() }, CapabilityScope::default()),
        ];

        for (req, expected) in &cases {
            let scope: CapabilityScope = req.into();
            assert_eq!(
                scope, *expected,
                "mismatch for {req:?}: expected {expected:?}, got {scope:?}",
            );
        }
    }

    // --- check_path_allowed ---

    /// `check_path_allowed` must reject relative paths regardless of grant state.
    #[test]
    fn test_check_path_allowed_rejects_relative_path() {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());

        let result = check_path_allowed(&caps, "p", "relative/path/file.rs", false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("only absolute paths"),
            "expected error about absolute paths, got: {err_msg}",
        );
    }

    // --- check_url_allowed ---

    /// A URL that has no host component must be rejected (domain-restricted case).
    #[test]
    fn test_check_url_allowed_no_host() {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(
            CapabilityDiscriminant::NetworkOutbound,
            CapabilityScope { domains: vec!["example.com".into()], ..Default::default() },
        );

        // "https://" has no host — URL parsing succeeds but host is empty.
        // With domain restrictions, URL parsing is triggered so we get the error.
        let result = check_url_allowed(&caps, "p", "https://");
        assert!(result.is_err());
    }

    /// An empty URL must be rejected when domain restrictions are active.
    #[test]
    fn test_check_url_allowed_empty_with_domains() {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(
            CapabilityDiscriminant::NetworkOutbound,
            CapabilityScope { domains: vec!["example.com".into()], ..Default::default() },
        );

        // Empty string — becomes "https://" which has no host.
        let result = check_url_allowed(&caps, "p", "");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Network egress allowlist (threat model §6 gap #7)
    // ------------------------------------------------------------------

    /// Grant `NetworkOutbound` with exactly `domains` as the egress allowlist.
    fn granted_egress(domains: &[&str]) -> GrantedCapabilities {
        let mut caps = GrantedCapabilities::new();
        caps.grant_session(
            CapabilityDiscriminant::NetworkOutbound,
            CapabilityScope {
                domains: domains.iter().map(|d| (*d).to_string()).collect(),
                ..Default::default()
            },
        );
        caps
    }

    /// A configured allowlist admits a target that matches on scheme, host
    /// and port simultaneously.
    #[test]
    fn egress_allowlist_permits_listed_scheme_host_port() {
        let caps = granted_egress(&["https://api.example.com:443"]);
        assert!(check_url_allowed(&caps, "p", "https://api.example.com:443/v1").is_ok());
    }

    /// A target matching no entry is refused, and the error names the rule
    /// that refused it plus the offending host.
    #[test]
    fn egress_allowlist_denies_unlisted_host_naming_rule() {
        let caps = granted_egress(&["https://api.example.com:443"]);
        let err = check_url_allowed(&caps, "p", "https://evil.example.net/exfil")
            .unwrap_err()
            .to_string();
        assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
        assert!(err.contains("evil.example.net"), "error must name the target, got: {err}");
    }

    /// A scheme-qualified rule refuses the same host over another scheme.
    #[test]
    fn egress_allowlist_denies_scheme_mismatch() {
        let caps = granted_egress(&["https://example.com"]);
        let err = check_url_allowed(&caps, "p", "http://example.com/x").unwrap_err().to_string();
        assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
    }

    /// A port-qualified rule refuses the same host on another port (targets
    /// without an explicit port are compared against their effective port).
    #[test]
    fn egress_allowlist_denies_port_mismatch() {
        let caps = granted_egress(&["example.com:8443"]);
        let err = check_url_allowed(&caps, "p", "https://example.com/").unwrap_err().to_string();
        assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
    }

    /// Unconfigured (`domains == []`) keeps today's fail-open behaviour: any
    /// target passes, including ones a configured allowlist would refuse —
    /// even when the target does not parse at all (parsing happens only
    /// after the unconfigured fast path, exactly as before).
    #[test]
    fn egress_unconfigured_fails_open_for_any_target() {
        let caps = granted_egress(&[]);
        assert!(check_url_allowed(&caps, "p", "https://anything.example.org:8443/x").is_ok());
        assert!(check_url_allowed(&caps, "p", "not a url at all").is_ok());
    }

    /// A bare-host entry keeps its pre-gap-#7 meaning: any scheme, any port,
    /// apex or subdomain.
    #[test]
    fn egress_bare_host_rule_matches_any_scheme_and_port() {
        let caps = granted_egress(&["example.com"]);
        assert!(check_url_allowed(&caps, "p", "http://example.com:9999/x").is_ok());
        assert!(check_url_allowed(&caps, "p", "https://api.example.com/").is_ok());
        assert!(check_url_allowed(&caps, "p", "https://evil.com/").is_err());
    }

    /// A leading `*.` is accepted and means apex-or-subdomain (it used to be
    /// a dead entry that matched nothing).
    #[test]
    fn egress_star_prefix_matches_apex_and_subdomains() {
        let caps = granted_egress(&["*.example.com"]);
        assert!(check_url_allowed(&caps, "p", "https://api.example.com/").is_ok());
        assert!(check_url_allowed(&caps, "p", "https://example.com/").is_ok());
        assert!(check_url_allowed(&caps, "p", "https://notexample.com/").is_err());
    }

    /// A bare `*` is not a wildcard: it never matches, so a malformed rule
    /// stays fail-closed instead of widening to "allow everything".
    #[test]
    fn egress_bare_star_entry_denies_everything() {
        let caps = granted_egress(&["*"]);
        let err = check_url_allowed(&caps, "p", "https://example.com/").unwrap_err().to_string();
        assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
    }

    /// Without the coarse capability the allowlist is never consulted; the
    /// refusal names the capability rule instead.
    #[test]
    fn egress_ungranted_capability_names_network_capability_rule() {
        let caps = GrantedCapabilities::new();
        let err = check_url_allowed(&caps, "p", "https://example.com/").unwrap_err().to_string();
        assert!(err.contains(RULE_NETWORK_CAPABILITY), "error must name the rule, got: {err}");
    }

    /// Malformed entries parse to `None` (never a match); well-formed ones
    /// in every supported shape parse.
    #[test]
    fn egress_rule_parse_rejects_malformed_entries() {
        for entry in ["", "   ", "example.com:", "example.com:notaport", "*", "*.", "://host"] {
            assert!(EgressRule::parse(entry).is_none(), "expected {entry:?} to be rejected");
        }
        for entry in [
            "example.com",
            "*.example.com",
            "https://example.com",
            "*://example.com",
            "example.com:8443",
            "https://example.com:443",
            "[::1]:8080",
            "::1",
        ] {
            assert!(EgressRule::parse(entry).is_some(), "expected {entry:?} to parse");
        }
    }

    /// Scheme/port constraints only apply when the rule names them.
    #[test]
    fn egress_rule_matching_dimensions() {
        let target = extract_egress_target("https://example.com:443/x").expect("valid url");
        let admits = |entry: &str| EgressRule::parse(entry).expect("valid rule").matches(&target);
        assert!(admits("example.com"), "host-only rule ignores scheme and port");
        assert!(admits("*.example.com"));
        assert!(admits("https://example.com"));
        assert!(admits("example.com:443"));
        assert!(admits("*://example.com:443"));
        assert!(!admits("http://example.com"), "scheme mismatch must refuse");
        assert!(!admits("example.com:8443"), "port mismatch must refuse");
        assert!(!admits("other.example.com"), "host mismatch must refuse");
    }

    // ------------------------------------------------------------------
    // ADR-37 — grant lifecycle: TTL expiry, hash pinning, legacy migration,
    // revocation (exercised through the public `CapabilityManager` API by
    // writing `plugin_cap_grants.json` directly into a temp data dir).
    // ------------------------------------------------------------------

    /// Write `json` as `plugin_cap_grants.json` inside `dir` (creating it).
    fn write_grants_json(dir: &std::path::Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("plugin_cap_grants.json"), json).unwrap();
    }

    #[test]
    fn grant_ttl_expiry_filters_grant() {
        let dir = std::env::temp_dir().join("cap_ttl_expiry_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(
            &dir,
            r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":1,"manifest_hash":"abc"}]}"#,
        );
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        let grants = cap_mgr.load_grants("my-plugin", None);
        assert!(grants.is_empty(), "grant past its TTL must be filtered out");
    }

    #[test]
    fn grant_within_ttl_loads() {
        let dir = std::env::temp_dir().join("cap_ttl_within_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(
            &dir,
            r#"{"my-plugin":[{"disc":"FilesystemRead","globs":["src/**/*.rs"],"domains":[],"allowlist":[],"created_at":0,"expires_at":18446744073709551615,"manifest_hash":"abc"}]}"#,
        );
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        let grants = cap_mgr.load_grants("my-plugin", None);
        assert_eq!(grants.len(), 1, "non-expired grant must load");
        assert_eq!(grants[0].0, CapabilityDiscriminant::FilesystemRead);
        assert_eq!(grants[0].1.globs, vec!["src/**/*.rs"], "scope globs must round-trip");
        assert_eq!(
            grants[0].2, 18_446_744_073_709_551_615,
            "expires_at must round-trip into the in-memory model"
        );
    }

    #[test]
    fn hash_mismatch_filters_grant() {
        // Each behavior uses its own store file: prune-on-load destroys the
        // mismatched grant, so a single store cannot serve all three checks.
        let grants_json = r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}]}"#;

        // Current hash differs from the pinned one → stale, filtered out.
        let dir = std::env::temp_dir().join("cap_hash_mismatch_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(&dir, grants_json);
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        assert!(cap_mgr.load_grants("my-plugin", Some("def")).is_empty());

        // Matching hash → grant loads (from a fresh, un-pruned store).
        let dir = std::env::temp_dir().join("cap_hash_match_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(&dir, grants_json);
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        assert_eq!(cap_mgr.load_grants("my-plugin", Some("abc")).len(), 1);

        // `None` skips hash pinning entirely → grant loads.
        let dir = std::env::temp_dir().join("cap_hash_none_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(&dir, grants_json);
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        assert_eq!(cap_mgr.load_grants("my-plugin", None).len(), 1);
    }

    #[test]
    fn hash_mismatch_pruned_from_disk() {
        let dir = std::env::temp_dir().join("cap_hash_mismatch_prune_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(
            &dir,
            r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}]}"#,
        );
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        // Loading with a differing hash prunes the stale grant (ADR-37
        // prune-on-load): a fresh manager over the same file must see nothing.
        assert!(cap_mgr.load_grants("my-plugin", Some("def")).is_empty());
        drop(cap_mgr);
        let reopened = CapabilityManager::open(&dir).unwrap();
        assert!(
            reopened.load_grants("my-plugin", None).is_empty(),
            "the hash-mismatched grant must be removed from disk after first load"
        );
        assert!(
            !reopened.list_granted_plugins().contains(&"my-plugin".to_string()),
            "pruned plugin must not be listed as granted"
        );
    }

    #[test]
    fn legacy_grant_format_migrates_with_ttl() {
        let dir = std::env::temp_dir().join("cap_legacy_migration_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(&dir, r#"{"my-plugin":["FilesystemRead","BogusCap"]}"#);
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        let grants = cap_mgr.load_grants("my-plugin", None);
        assert_eq!(
            grants.len(),
            1,
            "only valid discriminants survive legacy migration (BogusCap dropped)"
        );
        assert_eq!(grants[0].0, CapabilityDiscriminant::FilesystemRead);
    }

    #[test]
    fn revoke_plugin_removes_grants() {
        let dir = std::env::temp_dir().join("cap_revoke_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(
            &dir,
            r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}]}"#,
        );
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        assert_eq!(cap_mgr.load_grants("my-plugin", None).len(), 1);
        cap_mgr.revoke_plugin("my-plugin").unwrap();
        assert!(cap_mgr.load_grants("my-plugin", None).is_empty());
        assert!(
            !cap_mgr.list_granted_plugins().contains(&"my-plugin".to_string()),
            "revoked plugin must not be listed"
        );
    }

    #[test]
    fn list_granted_plugins_skips_expired() {
        let dir = std::env::temp_dir().join("cap_list_plugins_test");
        let _ = std::fs::remove_dir_all(&dir);
        write_grants_json(
            &dir,
            r#"{"valid-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}],"expired-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":1,"manifest_hash":"abc"}]}"#,
        );
        let cap_mgr = CapabilityManager::open(&dir).unwrap();
        let listed = cap_mgr.list_granted_plugins();
        assert!(listed.contains(&"valid-plugin".to_string()));
        assert!(!listed.contains(&"expired-plugin".to_string()));
    }
}
