use std::path::Path;

use crate::error::PluginError;

/// A discovered plugin candidate on the filesystem.
#[derive(Debug, Clone)]
pub struct PluginCandidate {
    pub wasm_path: std::path::PathBuf,
    pub sidecar_manifest_path: Option<std::path::PathBuf>,
}

/// Configuration for plugin discovery.
pub struct DiscoveryConfig {
    /// Directories to search for plugins.
    pub search_paths: Vec<std::path::PathBuf>,
    /// Also scan bundled plugin directory.
    pub bundled_path: Option<std::path::PathBuf>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        let mut search_paths = Vec::new();
        if let Some(data_dir) = dirs_data_dir() {
            search_paths.push(data_dir.join("plugins"));
        }
        Self { search_paths, bundled_path: None }
    }
}

/// Plugin discovery — scans filesystem directories for `.wasm` files.
pub struct PluginDiscovery {
    config: DiscoveryConfig,
}

impl PluginDiscovery {
    pub fn new(config: DiscoveryConfig) -> Self {
        Self { config }
    }

    /// Scan configured directories for plugin candidates.
    pub fn discover(&self) -> Result<Vec<PluginCandidate>, PluginError> {
        let mut candidates = Vec::new();
        for path in &self.config.search_paths {
            if !path.exists() {
                continue;
            }
            for entry in std::fs::read_dir(path).map_err(PluginError::Io)? {
                let entry = entry.map_err(PluginError::Io)?;
                let p = entry.path();
                if p.extension().is_some_and(|e| e == "wasm") {
                    let sidecar = find_sidecar_manifest(&p);
                    candidates
                        .push(PluginCandidate { wasm_path: p, sidecar_manifest_path: sidecar });
                }
            }
        }
        if let Some(bundled) = &self.config.bundled_path {
            if bundled.exists() {
                for entry in std::fs::read_dir(bundled).map_err(PluginError::Io)? {
                    let entry = entry.map_err(PluginError::Io)?;
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "wasm") {
                        let sidecar = find_sidecar_manifest(&p);
                        candidates
                            .push(PluginCandidate { wasm_path: p, sidecar_manifest_path: sidecar });
                    }
                }
            }
        }
        Ok(candidates)
    }
}

/// Find a sidecar manifest next to a `.wasm` file.
///
/// Looks for `.manifest.json` first (preferred), then `.toml` (legacy).
/// If both exist, `.manifest.json` wins.
pub fn find_sidecar_manifest(wasm_path: &Path) -> Option<std::path::PathBuf> {
    let json_sidecar = wasm_path.with_extension("manifest.json");
    if json_sidecar.exists() {
        return Some(json_sidecar);
    }
    let toml_sidecar = wasm_path.with_extension("toml");
    if toml_sidecar.exists() {
        Some(toml_sidecar)
    } else {
        None
    }
}

/// Get the XDG data directory with legacy fallback.
///
/// Returns the new path (`~/.local/share/concerto/`) if it exists,
/// falling back to the old path (`~/.local/share/opencode-rs/`) for
/// installations that haven't migrated yet. This ensures plugins installed
/// under the old name are still discovered.
fn dirs_data_dir() -> Option<std::path::PathBuf> {
    const NEW_DATA_DIR: &str = "concerto";
    const OLD_DATA_DIR: &str = "opencode-rs";

    let dir = dirs::data_dir()?;

    let new_dir = dir.join(NEW_DATA_DIR);
    if new_dir.exists() {
        return Some(new_dir);
    }

    let old_dir = dir.join(OLD_DATA_DIR);
    if old_dir.exists() {
        return Some(old_dir);
    }

    Some(new_dir)
}
