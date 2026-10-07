//! Capability value types: discriminants, scopes, in-memory grants, and
//! the approval-UI trait.

use std::collections::HashMap;

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
    /// pre-existing fail-open behaviour
    /// (see [`check_url_allowed`](crate::capability::check_url_allowed)).
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

/// Returns the current Unix timestamp in seconds.
pub(super) fn now_unix() -> u64 {
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

#[cfg(test)]
mod tests;
