//! Per-call capability policy checks for host functions: filesystem
//! paths, network egress, and shell commands.

use concerto_api_types::plugin::CapabilityRequest;

use crate::error::PluginError;

use super::egress::{extract_egress_target, match_egress_allowlist};
use super::glob::glob_match;
use super::types::{CapabilityDiscriminant, GrantedCapabilities};

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
mod tests;
