//! Capability types, policy checks, and grant-store lifecycle for plugins.
//!
//! The module is split by concern — `types` (grant/scope value types),
//! `policy` (per-call path/URL/shell enforcement, backed by the `glob` and
//! `egress` matchers), `store` (the file-backed persisted grant store), and
//! `manager` ([`CapabilityManager`] lifecycle) — and re-exports every public
//! item here so the historical `capability::*` paths compile unchanged.

mod egress;
mod glob;
mod manager;
mod policy;
mod store;
mod types;

pub use manager::CapabilityManager;
pub use policy::{
    check_path_allowed, check_shell_allowed, check_url_allowed, RULE_EGRESS_ALLOWLIST,
    RULE_NETWORK_CAPABILITY,
};
pub use types::{
    sha256_hex, CapabilityApprovalUI, CapabilityDiscriminant, CapabilityScope, DenyUnapproved,
    GrantDecision, GrantPruneReport, GrantedCapabilities,
};
