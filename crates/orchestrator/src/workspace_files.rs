//! Bounded, revision-checked operator file operations through the shared executor.
use base64::Engine;
use concerto_core::{CancellationToken, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::harness_service::{HarnessService, HarnessServiceError};

pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDocument {
    pub path: String,
    pub content: String,
    pub revision: String,
}

fn refused(message: impl Into<String>) -> HarnessServiceError {
    ToolError::ExecutionFailed { message: message.into() }.into()
}

impl HarnessService {
    /// Directory entries retain the filesystem tool's explicit truncation flag.
    pub async fn workspace_list(
        &self,
        path: &str,
        cancel: CancellationToken,
    ) -> Result<Value, HarnessServiceError> {
        Ok(self.invoke_executor("filesystem", json!({"operation": "list", "path": path}), cancel).await?.data)
    }

    /// Read complete UTF-8 text or fail; never return a truncated editable buffer.
    pub async fn workspace_read(
        &self,
        path: &str,
        cancel: CancellationToken,
    ) -> Result<WorkspaceDocument, HarnessServiceError> {
        let result = self.invoke_executor("filesystem", json!({
            "operation": "read", "path": path, "max_bytes": MAX_DOCUMENT_BYTES,
        }), cancel).await?;
        let content = result.data["content"].as_str().ok_or_else(|| refused("filesystem returned no text"))?.to_owned();
        let revision = blake3::hash(content.as_bytes()).to_hex().to_string();
        Ok(WorkspaceDocument { path: path.into(), content, revision })
    }

    /// `expected = None` means create-only. Otherwise require the displayed
    /// revision before applying an edit. This detects stale drafts, not a
    /// cross-process compare-and-swap: external writers still require coordination.
    pub async fn workspace_write(
        &self,
        path: &str,
        content: &str,
        expected: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<Value, HarnessServiceError> {
        if content.len() > MAX_DOCUMENT_BYTES {
            return Err(refused("document exceeds 1 MiB limit"));
        }
        match expected {
            Some(revision) => self.workspace_check_revision(path, revision, cancel.clone()).await?,
            None => {
                let result = self.invoke_executor("filesystem", json!({"operation": "exists", "path": path}), cancel.clone()).await?;
                if result.data["exists"] != false {
                    return Err(refused("create refused: path already exists"));
                }
            }
        }
        // Human edits must retain smart quotes, CRLF and every UTF-8 byte.
        // The model-text channel intentionally normalizes typography.
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        Ok(self.invoke_executor("filesystem", json!({
            "operation": "write", "path": path, "content_b64": encoded,
        }), cancel).await?.data)
    }

    pub async fn workspace_delete(
        &self,
        path: &str,
        expected: &str,
        cancel: CancellationToken,
    ) -> Result<Value, HarnessServiceError> {
        self.workspace_check_revision(path, expected, cancel.clone()).await?;
        Ok(self.invoke_executor("filesystem", json!({"operation": "delete", "path": path}), cancel).await?.data)
    }

    async fn workspace_check_revision(
        &self,
        path: &str,
        expected: &str,
        cancel: CancellationToken,
    ) -> Result<(), HarnessServiceError> {
        let current = self.workspace_read(path, cancel).await?;
        if current.revision != expected {
            return Err(refused("file changed since it was read; reload and review before saving"));
        }
        Ok(())
    }

    /// Literal, case-sensitive search over explicitly selected files. Failed
    /// reads remain errors: an inaccessible file cannot masquerade as no matches.
    pub async fn workspace_search(
        &self,
        paths: &[String],
        query: &str,
        cancel: CancellationToken,
    ) -> Result<Value, HarnessServiceError> {
        if query.is_empty() || paths.is_empty() || paths.len() > 32 {
            return Err(refused("search requires a nonempty query and 1..32 files"));
        }
        let mut matches = Vec::new();
        for path in paths {
            let document = self.workspace_read(path, cancel.clone()).await?;
            for (line, text) in document.content.lines().enumerate() {
                if cancel.is_cancelled() { return Err(ToolError::Cancelled.into()); }
                if text.contains(query) {
                    if matches.len() == 1000 {
                        return Ok(json!({"matches": matches, "truncated": true}));
                    }
                    // Unicode scalar column for human display, one-based.
                    let byte = text.find(query).unwrap_or(0);
                    matches.push(json!({"path": path, "line": line + 1, "column": text[..byte].chars().count() + 1, "text": text}));
                }
            }
        }
        Ok(json!({"matches": matches, "truncated": false}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use concerto_core::executor::ToolExecutor;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::types::{Condition, PolicyRule, SessionContext, ToolRegistry};
    use concerto_tools::filesystem::FilesystemTool;

    struct Audit;
    #[async_trait::async_trait]
    impl AuditLog for Audit {
        async fn record(&self, _: AuditEntry, _: CancellationToken) -> Result<(), concerto_core::error::PolicyError> { Ok(()) }
    }

    fn service(root: &std::path::Path, deny: bool) -> HarnessService {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FilesystemTool::new(camino::Utf8PathBuf::from_path_buf(root.to_owned()).unwrap())));
        let rule = if deny { PolicyRule::Deny(Condition::Always) } else { PolicyRule::AutoApprove(Condition::Always) };
        let policy = Arc::new(SimplePolicyEngine::new(vec![rule], Arc::new(Audit)));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy));
        HarnessService::new(executor, SessionContext::new(concerto_core::ids::Ulid::new(), root.to_owned()))
    }

    #[tokio::test]
    async fn edits_preserve_bytes_reject_stale_drafts_and_do_not_bypass_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let text = "“Hello” 世界\r\n\tsecond line\r\n";
        service(dir.path(), false).workspace_write("file.txt", text, None, CancellationToken::new()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        let document = service(dir.path(), false).workspace_read("file.txt", CancellationToken::new()).await.unwrap();
        assert_eq!(document.content, text);
        assert!(service(dir.path(), false).workspace_write("file.txt", "overwrite", None, CancellationToken::new()).await.is_err());
        assert!(service(dir.path(), true).workspace_write("file.txt", "denied", Some(&document.revision), CancellationToken::new()).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::write(&path, "external edit").unwrap();
        assert!(service(dir.path(), false).workspace_write("file.txt", "stale", Some(&document.revision), CancellationToken::new()).await.is_err());
        assert!(service(dir.path(), false).workspace_delete("file.txt", &document.revision, CancellationToken::new()).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");
        let document = service(dir.path(), false).workspace_read("file.txt", CancellationToken::new()).await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(service(dir.path(), false).workspace_delete("file.txt", &document.revision, cancel).await.is_err());
        assert!(path.exists());
        service(dir.path(), false).workspace_delete("file.txt", &document.revision, CancellationToken::new()).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn reads_are_complete_bounded_and_search_reports_real_locations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "before\n界 match\nlast").unwrap();
        let harness = service(dir.path(), false);
        let result = harness.workspace_search(&["a".into()], "match", CancellationToken::new()).await.unwrap();
        assert_eq!(result["matches"][0]["line"], 2);
        assert_eq!(result["matches"][0]["column"], 3);
        assert_eq!(result["truncated"], false);
        std::fs::write(dir.path().join("large"), vec![b'x'; MAX_DOCUMENT_BYTES + 1]).unwrap();
        assert!(harness.workspace_read("large", CancellationToken::new()).await.is_err());
        std::fs::write(dir.path().join("binary"), [0xff, 0xfe]).unwrap();
        assert!(harness.workspace_read("binary", CancellationToken::new()).await.is_err());
        assert!(harness.workspace_read("../outside", CancellationToken::new()).await.is_err());
        assert!(harness.workspace_search(&["missing".into()], "match", CancellationToken::new()).await.is_err());
    }
}
