use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// Immutable context made available to a command invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellContext {
    pub project_root: Utf8PathBuf,
    pub cwd: Utf8PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub agent_roles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl ShellContext {
    #[must_use]
    pub fn new(project_root: Utf8PathBuf) -> Self {
        Self {
            cwd: project_root.clone(),
            project_root,
            project_id: None,
            session_id: None,
            provider: None,
            model: None,
            agent_roles: Vec::new(),
            branch: None,
        }
    }
}
