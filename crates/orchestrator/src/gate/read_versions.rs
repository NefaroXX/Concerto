//! Read-derived claims, durable for identified sessions. No disk reread may replace
//! the bytes returned to an agent with a newer, unobserved base.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use super::{versioned_targets, GateRequest};

pub(super) const ABSENT: &str = "<absent>";
pub(super) const REREAD: &str = "<read-required>";
const MAX_OBSERVATIONS: usize = 4096;

mod durable;

type Key = (Option<String>, String, PathBuf);

#[derive(Default)]
pub(super) struct ReadVersions {
    versions: HashMap<Key, String>,
    saturated: bool,
}

impl ReadVersions {
    pub(super) fn observed(&self, root: &Path, req: &GateRequest, path: &str) -> Option<String> {
        for path in path_keys(root, path) {
            if let Some(version) = self.versions.get(&key(req, path)) {
                return Some(version.clone());
            }
        }
        // Never evict a stale observation and silently revert to arrival-time
        // stamping. An overflowing run must reread a tracked path or restart.
        self.saturated.then(|| REREAD.to_owned())
    }

    pub(super) fn record_read(&mut self, root: &Path, req: &GateRequest, data: &serde_json::Value) {
        let Some((path, version)) = read_observation(req, data) else { return };
        self.insert(root, req, path, &version);
    }

    pub(super) fn record_write(
        &mut self,
        root: &Path,
        req: &GateRequest,
        content_was_staged: bool,
    ) {
        for path in versioned_targets(req) {
            // Writes without an earlier read retain their existing behavior.
            if self.observed(root, req, &path).is_none() {
                continue;
            }
            let version = written_version(req, &path, content_was_staged);
            self.insert(root, req, &path, &version);
        }
    }

    fn insert(&mut self, root: &Path, req: &GateRequest, path: &str, version: &str) {
        for path in path_keys(root, path) {
            let key = key(req, path);
            if self.versions.contains_key(&key) || self.versions.len() < MAX_OBSERVATIONS {
                self.versions.insert(key, version.to_owned());
            } else {
                self.saturated = true;
            }
        }
    }
}

fn read_observation<'a>(
    req: &'a GateRequest,
    data: &serde_json::Value,
) -> Option<(&'a str, String)> {
    if req.tool != "filesystem" {
        return None;
    }
    let path = req.input["path"].as_str()?;
    let version = match req.input["operation"].as_str() {
        Some("read") => match data.get("content_hash") {
            Some(hash) => hash
                .as_str()
                .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_owned)
                .unwrap_or_else(|| REREAD.to_owned()),
            None => data["content"]
                .as_str()
                .map(|content| blake3::hash(content.as_bytes()).to_hex().to_string())
                .unwrap_or_else(|| REREAD.to_owned()),
        },
        Some("exists") if data["exists"] == false => ABSENT.to_owned(),
        _ => return None,
    };
    Some((path, version))
}

fn written_version(req: &GateRequest, path: &str, content_was_staged: bool) -> String {
    match req.input["operation"].as_str() {
        Some("write") if !content_was_staged => req.input["content"]
            .as_str()
            .map(|content| blake3::hash(content.as_bytes()).to_hex().to_string())
            .unwrap_or_else(|| REREAD.to_owned()),
        Some("delete") => ABSENT.to_owned(),
        Some("move") if req.input["path"].as_str() == Some(path) => ABSENT.to_owned(),
        // A staged merge contains external edits the agent has not
        // seen. Like copy/move destinations and opaque binary writes,
        // it requires a fresh read. Never attest a later disk reread.
        _ => REREAD.to_owned(),
    }
}

fn key(req: &GateRequest, path: PathBuf) -> Key {
    (req.session_id.clone(), req.agent_id.clone(), path)
}

/// Keep the lexical spelling as well as a resolved alias: retargeting a
/// symlink must not discard the observation made through its original name.
fn path_keys(root: &Path, raw: &str) -> Vec<PathBuf> {
    let joined = root.join(raw);
    let mut lexical = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            _ => lexical.push(component.as_os_str()),
        }
    }
    let mut paths = vec![lexical.clone()];
    if let Ok(resolved) = joined.canonicalize() {
        if resolved != lexical {
            paths.push(resolved);
        }
    }
    paths
        .into_iter()
        .map(|path| match camino::Utf8Path::from_path(&path) {
            Some(utf8) => PathBuf::from(concerto_tools::common::path_comparison_key(utf8)),
            None => path,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_request(path: &str) -> GateRequest {
        GateRequest {
            call_id: "read".into(),
            agent_id: "agent".into(),
            tool: "filesystem".into(),
            input: json!({"operation":"read", "path":path}),
            registered_as: None,
            session_id: None,
            scope: "fs".into(),
            plan_id: None,
            causation: None,
            base_versions: Default::default(),
            orchestrator_authority: false,
        }
    }

    #[test]
    fn read_bound_capacity_never_evicts_protection() {
        let dir = tempfile::tempdir().unwrap();
        let mut versions = ReadVersions::default();
        for i in 0..=MAX_OBSERVATIONS {
            versions.record_read(
                dir.path(),
                &read_request(&format!("f{i}")),
                &json!({"content":"base"}),
            );
        }
        assert_eq!(versions.versions.len(), MAX_OBSERVATIONS);
        let req = read_request("f0");
        assert_eq!(
            versions.observed(dir.path(), &req, "f0"),
            Some(blake3::hash(b"base").to_hex().to_string())
        );
        assert_eq!(versions.observed(dir.path(), &req, "unknown"), Some(REREAD.into()));
        versions.record_read(dir.path(), &req, &json!({"content":"reread"}));
        assert_eq!(
            versions.observed(dir.path(), &req, "f0"),
            Some(blake3::hash(b"reread").to_hex().to_string())
        );
    }

    #[test]
    fn read_bound_unknown_mutation_requires_reread_without_hashing_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut versions = ReadVersions::default();
        let mut req = read_request("dest");
        versions.record_read(dir.path(), &req, &json!({"content":"original"}));
        req.input = json!({"operation":"copy", "path":"src", "destination":"dest"});
        std::fs::write(dir.path().join("dest"), "external after copy").unwrap();
        versions.record_write(dir.path(), &req, false);
        assert_eq!(versions.observed(dir.path(), &req, "dest"), Some(REREAD.into()));
        req.input = json!({"operation":"read", "path":"dest"});
        versions.record_read(dir.path(), &req, &json!({"content":"fresh"}));
        assert_eq!(
            versions.observed(dir.path(), &req, "dest"),
            Some(blake3::hash(b"fresh").to_hex().to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn read_bound_windows_aliases_share_absence_observations() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = read_request("File.txt");
        req.input["operation"] = json!("exists");
        let mut versions = ReadVersions::default();
        versions.record_read(dir.path(), &req, &json!({"exists":false}));
        assert_eq!(versions.observed(dir.path(), &req, "file.TXT"), Some(ABSENT.into()));
        let absolute = dir.path().join("file.txt");
        assert_eq!(
            versions.observed(dir.path(), &req, absolute.to_str().unwrap()),
            Some(ABSENT.into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_bound_symlink_retarget_keeps_the_original_observation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "a").unwrap();
        std::fs::write(dir.path().join("b"), "b").unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("alias")).unwrap();
        let req = read_request("alias");
        let mut versions = ReadVersions::default();
        versions.record_read(dir.path(), &req, &json!({"content":"a"}));
        assert_eq!(
            versions.observed(dir.path(), &req, "a"),
            Some(blake3::hash(b"a").to_hex().to_string())
        );
        std::fs::remove_file(dir.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("b", dir.path().join("alias")).unwrap();
        assert_eq!(
            versions.observed(dir.path(), &req, "alias"),
            Some(blake3::hash(b"a").to_hex().to_string())
        );
        assert_eq!(
            versions.observed(dir.path(), &req, dir.path().join("alias").to_str().unwrap()),
            Some(blake3::hash(b"a").to_hex().to_string())
        );
    }
}
