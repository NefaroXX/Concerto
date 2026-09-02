//! Filesystem tool — exposes `VirtualFs` operations through the `Tool` trait,
//! policy-gated via the executor choke-point.

use crate::virtual_fs::{reject_reserved_device_name, VirtualFs};
use async_trait::async_trait;
use camino::Utf8Path;
use concerto_core::text::normalize_typographic;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::{CancellationToken, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing;

/// Parameter struct for the `filesystem` tool.
///
/// The schema is derived from this type via `schemars` so the advertised JSON
/// Schema contract can never drift from the actual deserialization target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesystemInput {
    /// The filesystem operation to perform: "read", "write", "delete",
    /// "exists", "list", "move", or "copy".
    ///
    /// The advertised enum lets the orchestrator's tool-call guard emit a
    /// `one of [...]` hint in corrective results when a weak model omits the
    /// field. Keep the list in sync with the operation match arms in
    /// [`FilesystemTool::execute`].
    #[schemars(extend("enum" = ["read", "write", "delete", "exists", "list", "move", "copy"]))]
    pub operation: String,
    /// Path relative to project root. Use "." to list the workspace root.
    pub path: String,
    /// Content to write (required for the "write" operation only).
    #[serde(default)]
    #[schemars(description = "Content to write (required for write operation).")]
    pub content: Option<String>,
    /// Destination path (required for "move" and "copy" operations).
    #[serde(default)]
    #[schemars(description = "Destination path (required for move/copy operations).")]
    pub destination: Option<String>,
}

// ---------------------------------------------------------------------------
// Lenient input coercion
// ---------------------------------------------------------------------------

/// Leniently binds a raw filesystem tool argument into [`FilesystemInput`].
///
/// The JSON Schema advertised via [`FilesystemTool::input_schema`] stays
/// authoritative; only the boundary parser is lenient, mirroring the git tool.
/// Strict deserialization is tried first, then per-field normalization and a
/// second strict parse. If the normalized input also fails, the ORIGINAL
/// strict deserialize error is returned (message shape
/// `invalid filesystem input: {e}` unchanged).
fn coerce_filesystem_input(input: &serde_json::Value) -> Result<FilesystemInput, ToolError> {
    match serde_json::from_value(input.clone()) {
        Ok(parsed) => Ok(parsed),
        Err(strict_error) => {
            let normalized = normalize_filesystem_input(input);
            serde_json::from_value(normalized).map_err(|_| ToolError::ExecutionFailed {
                message: format!("invalid filesystem input: {strict_error}"),
            })
        }
    }
}

/// Normalizes a filesystem tool input after strict parsing has failed.
///
/// `path`, `operation`, and `destination` accept non-string scalars
/// (numbers/bools → strings). `content` also accepts numbers/bools but must
/// stay string-or-null: objects are never coerced and pass through so the
/// strict parser rejects them with the original diagnostic.
fn normalize_filesystem_input(input: &serde_json::Value) -> serde_json::Value {
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let mut normalized = object.clone();
    for (field, value) in normalized.iter_mut() {
        match field.as_str() {
            "operation" | "path" | "content" | "destination" => {
                *value = coerce_scalar_to_string(value);
            }
            _ => {}
        }
    }
    serde_json::Value::Object(normalized)
}

/// Coerces a scalar non-string value into its string form for string fields.
///
/// Numbers and bools are converted; strings, `null`, arrays, and objects pass
/// through untouched (so optional fields can stay `null` and malformed fields
/// keep producing the strict deserialize error).
fn coerce_scalar_to_string(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(_) | serde_json::Value::Null => value.clone(),
        serde_json::Value::Number(number) => serde_json::json!(number.to_string()),
        serde_json::Value::Bool(boolean) => serde_json::json!(boolean.to_string()),
        other => other.clone(),
    }
}

/// Filesystem tool wrapping a `VirtualFs` instance.
///
/// Uses `Arc<Mutex<VirtualFs>>` internally so the same underlying VFS can
/// be shared between the tool (for agent writes) and the desktop diff viewer
/// (for hunk-accept/reject reconciliation).
pub struct FilesystemTool {
    root: camino::Utf8PathBuf,
    vfs: Arc<Mutex<VirtualFs>>,
}

impl FilesystemTool {
    /// Creates a new `FilesystemTool` rooted at the session's project directory.
    /// The `VirtualFs` is initialised lazily on first execute.
    pub fn new(root: camino::Utf8PathBuf) -> Self {
        Self { root, vfs: Arc::new(Mutex::new(VirtualFs::new())) }
    }

    /// Creates a new `FilesystemTool` that shares an existing `VirtualFs` instance.
    ///
    /// This is used by the desktop app so the diff viewer can reconcile hunk
    /// decisions (accept/reject) against the same `VirtualFs` that the agent
    /// wrote to during execution.
    pub fn new_shared(root: camino::Utf8PathBuf, vfs: Arc<Mutex<VirtualFs>>) -> Self {
        Self { root, vfs }
    }

    /// Returns a reference to the shared VirtualFs.
    pub fn vfs(&self) -> &Arc<Mutex<VirtualFs>> {
        &self.vfs
    }
}

/// Return `ToolError::Cancelled` if the token has been cancelled.
fn check_cancel(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

#[async_trait]
impl concerto_core::traits::tool::Tool for FilesystemTool {
    fn name(&self) -> &str {
        "filesystem"
    }

    fn description(&self) -> &str {
        "Read, write, delete, list, and check files within the project workspace. Use list, not read, for directories."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Derive the schema from the Rust type so the advertised contract can
        // never drift from the deserialization target.  `operation` and `path`
        // (no default) are required; `content` (`Option`) is optional.
        let root = schemars::schema_for!(FilesystemInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize FilesystemInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "enum": ["read", "write", "delete", "exists", "list", "move", "copy"] },
                    "path": { "type": "string", "description": "Path relative to project root." },
                    "content": { "type": ["string", "null"], "description": "Content to write (required for write operation)." }
                },
                "required": ["operation", "path"]
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // Coarse flag vocabulary: agents declare `filesystem` when they may
        // use the filesystem at all (read or write). Fine-grained read/write
        // enforcement is policy's job (default rules auto-approve reads and
        // require approval for writes), not the capability filter's.
        CapabilitySet::default().with_requirement("filesystem")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        // Use the session's project_dir as the root, falling back to the
        // tool's stored root (set at construction time).
        let root = if !session.project_dir.as_os_str().is_empty() {
            Utf8Path::new(session.project_dir.to_str().ok_or_else(|| {
                ToolError::ExecutionFailed {
                    message: "session project_dir is not valid UTF-8".into(),
                }
            })?)
        } else {
            &self.root
        };

        // Leniently coerce the raw argument at the execution boundary: the
        // schema stays strict, but the parser accepts string-typed scalars
        // from the model.
        let fs_input: FilesystemInput = coerce_filesystem_input(&input)?;

        check_cancel(&cancel)?;

        let operation = &fs_input.operation;
        let path_str = &fs_input.path;
        check_cancel(&cancel)?;
        let path = crate::common::resolve_path(root, Utf8Path::new(path_str))?;

        check_cancel(&cancel)?;
        let mut vfs = self.vfs.lock().map_err(|e| ToolError::ExecutionFailed {
            message: format!("vfs lock poisoned: {e}"),
        })?;

        match operation.as_str() {
            "read" => {
                check_cancel(&cancel)?;
                // A reserved Windows device name is never a readable file: on
                // Windows the bare name addresses the device, and a `\\?\`
                // extended path can address a literal `nul` file. Reject it
                // explicitly instead of touching either.
                reject_reserved_device_name(&path)?;
                if path.is_dir() {
                    return Err(ToolError::ExecutionFailed {
                        message: format!(
                            "'{path_str}' is a directory in workspace '{root}'; use the filesystem list operation to inspect directories"
                        ),
                    });
                }
                // First try the in-memory VFS; if not found, load from disk.
                let content = if vfs.exists(&path) {
                    vfs.read(&path)?
                } else {
                    vfs.read_disk(&path).map_err(|error| {
                        if matches!(&error, ToolError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                        {
                            ToolError::ExecutionFailed {
                                message: format!(
                                    "'{path_str}' does not exist in workspace '{root}'. Check the path, or use the filesystem write operation to create it"
                                ),
                            }
                        } else {
                            error
                        }
                    })?
                };
                Ok(ToolOutput {
                    summary: format!("Read {} bytes from {path_str}", content.len()),
                    data: serde_json::json!({ "content": content, "path": path_str }),
                })
            }
            "write" => {
                check_cancel(&cancel)?;
                // Never materialize a reserved Windows device name as a real
                // file: the `\\?\` extended-path prefix bypasses Windows'
                // reserved-name check, so `std::fs::write` to `<root>\nul`
                // would create a literal 0-byte file instead of erroring.
                reject_reserved_device_name(&path)?;
                let raw_content =
                    fs_input.content.as_deref().ok_or_else(|| ToolError::ExecutionFailed {
                        message: "missing 'content' field for write operation".into(),
                    })?;
                // Normalize typographic Unicode at the write boundary so
                // model-produced content can never land smart quotes,
                // non-breaking hyphens, en dashes, or arrows in source files
                // (they break `cargo build` and other ASCII-delimited
                // tooling). The normalizer returns a borrow when the input is
                // already clean, so plain content is zero-allocation.
                let content = normalize_typographic(raw_content);
                let content_len = content.len();

                // Capture the original contents before materializing the write.
                // Otherwise an existing file is misclassified as Created and
                // rejecting the change can delete the user's original file.
                if !vfs.exists(&path) && path.as_std_path().is_file() {
                    vfs.read_disk(&path)?;
                }

                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent.as_std_path()).map_err(|e| {
                        ToolError::ExecutionFailed {
                            message: format!(
                                "failed to create parent directory for '{path_str}' in workspace '{root}': {e}"
                            ),
                        }
                    })?;
                }

                std::fs::write(path.as_std_path(), content.as_ref()).map_err(|e| {
                    ToolError::ExecutionFailed {
                        message: format!(
                            "failed to write '{path_str}' at '{path}' in workspace '{root}': {e}. Check that the selected project folder is writable and is not blocked by Windows Controlled Folder Access"
                        ),
                    }
                })?;

                vfs.write(&path, content.into_owned())?;

                Ok(ToolOutput {
                    summary: format!("Wrote {content_len} bytes to {path_str}"),
                    data: serde_json::json!({
                        "path": path_str,
                        "absolute_path": path.to_string(),
                        "size": content_len,
                        "materialized": true
                    }),
                })
            }
            "delete" => {
                check_cancel(&cancel)?;
                // A reserved device name is not a deletable file — on Windows
                // the bare name addresses the device; reject it explicitly.
                reject_reserved_device_name(&path)?;
                vfs.stage_delete(&path)?;

                if path.as_std_path().exists() {
                    std::fs::remove_file(path.as_std_path()).map_err(|e| {
                        ToolError::ExecutionFailed {
                            message: format!("failed to delete {path_str} from disk: {e}"),
                        }
                    })?;
                }

                Ok(ToolOutput {
                    summary: format!("Deleted {path_str}"),
                    data: serde_json::json!({
                        "path": path_str,
                        "absolute_path": path.to_string(),
                        "materialized": true
                    }),
                })
            }
            "exists" => {
                check_cancel(&cancel)?;
                let exists = vfs.exists(&path) || vfs.exists_on_disk(&path);
                Ok(ToolOutput {
                    summary: if exists {
                        format!("{path_str} exists")
                    } else {
                        format!("{path_str} does not exist")
                    },
                    data: serde_json::json!({ "path": path_str, "exists": exists }),
                })
            }
            "list" => {
                check_cancel(&cancel)?;
                if !path.is_dir() {
                    return Err(ToolError::ExecutionFailed {
                        message: format!(
                            "'{path_str}' is not a directory in workspace '{root}'; use read for files"
                        ),
                    });
                }

                check_cancel(&cancel)?;
                let mut entries = std::fs::read_dir(path.as_std_path())
                    .map_err(|error| ToolError::ExecutionFailed {
                        message: format!(
                            "failed to list '{path_str}' in workspace '{root}': {error}"
                        ),
                    })?
                    .filter_map(Result::ok)
                    .map(|entry| {
                        let kind = entry
                            .file_type()
                            .map(|file_type| {
                                if file_type.is_dir() {
                                    "directory"
                                } else if file_type.is_file() {
                                    "file"
                                } else if file_type.is_symlink() {
                                    "symlink"
                                } else {
                                    "other"
                                }
                            })
                            .unwrap_or("unknown");
                        serde_json::json!({
                            "name": entry.file_name().to_string_lossy(),
                            "kind": kind,
                        })
                    })
                    .collect::<Vec<_>>();
                entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
                const MAX_ENTRIES: usize = 500;
                let truncated = entries.len() > MAX_ENTRIES;
                entries.truncate(MAX_ENTRIES);

                Ok(ToolOutput {
                    summary: format!(
                        "Listed {} entr{} in {path_str}{}",
                        entries.len(),
                        if entries.len() == 1 { "y" } else { "ies" },
                        if truncated { " (truncated)" } else { "" }
                    ),
                    data: serde_json::json!({
                        "path": path_str,
                        "entries": entries,
                        "truncated": truncated,
                    }),
                })
            }
            "move" => {
                check_cancel(&cancel)?;
                let dest =
                    fs_input.destination.as_deref().ok_or_else(|| ToolError::ExecutionFailed {
                        message: "missing 'destination' field for move operation".into(),
                    })?;
                check_cancel(&cancel)?;
                let dest_path = crate::common::resolve_path(root, Utf8Path::new(dest))?;
                vfs.move_file(&path, &dest_path)?;
                // Also perform the real filesystem rename
                if path.as_std_path().exists() {
                    std::fs::rename(path.as_std_path(), dest_path.as_std_path()).map_err(|e| {
                        ToolError::ExecutionFailed {
                            message: format!("failed to move '{path_str}' to '{dest}': {e}"),
                        }
                    })?;
                }
                Ok(ToolOutput {
                    summary: format!("Moved {path_str} to {dest}"),
                    data: serde_json::json!({
                        "source": path_str,
                        "destination": dest,
                    }),
                })
            }
            "copy" => {
                check_cancel(&cancel)?;
                let dest =
                    fs_input.destination.as_deref().ok_or_else(|| ToolError::ExecutionFailed {
                        message: "missing 'destination' field for copy operation".into(),
                    })?;
                check_cancel(&cancel)?;
                let dest_path = crate::common::resolve_path(root, Utf8Path::new(dest))?;
                vfs.copy_file(&path, &dest_path)?;
                // Also perform the real filesystem copy
                if path.as_std_path().exists() {
                    std::fs::copy(path.as_std_path(), dest_path.as_std_path()).map_err(|e| {
                        ToolError::ExecutionFailed {
                            message: format!("failed to copy '{path_str}' to '{dest}': {e}"),
                        }
                    })?;
                }
                Ok(ToolOutput {
                    summary: format!("Copied {path_str} to {dest}"),
                    data: serde_json::json!({
                        "source": path_str,
                        "destination": dest,
                    }),
                })
            }
            other => Err(ToolError::ExecutionFailed {
                message: format!(
                    "unknown filesystem operation: {other}; valid operations: read, write, delete, exists, list, move, copy"
                ),
            }),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::AllowAllPolicy;
    use concerto_core::traits::tool::Tool;
    use tempfile::TempDir;

    fn tool_and_dir() -> (FilesystemTool, TempDir) {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (FilesystemTool::new(root), dir)
    }

    fn session_for(root: &std::path::Path) -> SessionContext {
        SessionContext::new(concerto_core::ids::Ulid::new(), root.to_path_buf())
    }

    fn test_policy() -> AllowAllPolicy {
        AllowAllPolicy
    }

    #[tokio::test]
    async fn read_write_roundtrip() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // Write
        let write_input = serde_json::json!({
            "operation": "write",
            "path": "hello.txt",
            "content": "Hello, World!"
        });
        let output = tool
            .execute(write_input, &policy, &session_for(dir.path()), cancel.clone())
            .await
            .unwrap();
        assert!(output.summary.contains("Wrote"));

        // Read back
        let read_input = serde_json::json!({
            "operation": "read",
            "path": "hello.txt"
        });
        let output =
            tool.execute(read_input, &policy, &session_for(dir.path()), cancel).await.unwrap();
        assert_eq!(output.data["content"], "Hello, World!");
    }

    #[tokio::test]
    async fn rejecting_write_to_existing_file_restores_original_disk_content() {
        let (tool, dir) = tool_and_dir();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "original").unwrap();

        tool.execute(
            serde_json::json!({
                "operation": "write",
                "path": "existing.txt",
                "content": "changed"
            }),
            &test_policy(),
            &session_for(dir.path()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed");

        let utf8_path = camino::Utf8PathBuf::from_path_buf(path.clone()).unwrap();
        let mut vfs = tool.vfs().lock().unwrap();
        assert!(matches!(
            vfs.get(&utf8_path),
            Some(crate::virtual_fs::VirtualFsEntry::Modified { original, current, .. })
                if original == "original" && current == "changed"
        ));
        vfs.reject_hunks(&utf8_path, &[0]).unwrap();
        vfs.materialize_paths(&[utf8_path]).unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }

    #[tokio::test]
    async fn exists_check() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // First write through the VFS so it knows about the file.
        let write_input = serde_json::json!({
            "operation": "write",
            "path": "existing.txt",
            "content": "data"
        });
        tool.execute(write_input, &policy, &session_for(dir.path()), cancel.clone()).await.unwrap();

        let input = serde_json::json!({
            "operation": "exists",
            "path": "existing.txt"
        });
        let output = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap();
        assert!(output.data["exists"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn delete_removes_file() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // First write through the VFS so it knows about the file.
        let write_input = serde_json::json!({
            "operation": "write",
            "path": "delete_me.txt",
            "content": "data"
        });
        tool.execute(write_input, &policy, &session_for(dir.path()), cancel.clone()).await.unwrap();

        let input = serde_json::json!({
            "operation": "delete",
            "path": "delete_me.txt"
        });
        let output = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap();
        assert!(output.summary.contains("Deleted"));
    }

    #[tokio::test]
    async fn unknown_operation_returns_error() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let input = serde_json::json!({
            "operation": "unknown",
            "path": "foo.txt"
        });
        let err = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
        let message = err.to_string();
        assert!(
            message.contains("unknown filesystem operation: unknown"),
            "unexpected message: {message}"
        );
        assert!(message.contains("valid operations:"), "unexpected message: {message}");
        assert!(message.contains("read"), "unexpected message: {message}");
        assert!(message.contains("write"), "unexpected message: {message}");
        assert!(message.contains("copy"), "unexpected message: {message}");
    }

    #[tokio::test]
    async fn missing_read_explains_how_to_recover() {
        let (tool, dir) = tool_and_dir();
        let input = serde_json::json!({
            "operation": "read",
            "path": "missing.rs"
        });
        let error = tool
            .execute(input, &test_policy(), &session_for(dir.path()), CancellationToken::new())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("does not exist"));
        assert!(error.to_string().contains("write operation"));
    }

    #[tokio::test]
    async fn reading_directory_recommends_list_instead_of_os_error() {
        let (tool, dir) = tool_and_dir();
        let input = serde_json::json!({
            "operation": "read",
            "path": "."
        });
        let error = tool
            .execute(input, &test_policy(), &session_for(dir.path()), CancellationToken::new())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("is a directory"));
        assert!(error.to_string().contains("list operation"));
    }

    #[tokio::test]
    async fn lists_workspace_directory() {
        let (tool, dir) = tool_and_dir();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let input = serde_json::json!({
            "operation": "list",
            "path": "."
        });

        let output = tool
            .execute(input, &test_policy(), &session_for(dir.path()), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(output.data["entries"][0]["name"], "Cargo.toml");
        assert_eq!(output.data["entries"][0]["kind"], "file");
        assert_eq!(output.data["entries"][1]["name"], "src");
        assert_eq!(output.data["entries"][1]["kind"], "directory");
    }

    #[tokio::test]
    async fn capability_requirements_returned() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let tool = FilesystemTool::new(root);
        let caps = tool.capability_requirements();
        // Just check it doesn't panic and returns something
        let _ = format!("{caps:?}");
    }

    #[tokio::test]
    async fn input_schema_is_valid() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let tool = FilesystemTool::new(root);
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(
            schema.get("properties").or(schema.get("$defs")).is_some(),
            "schema should have 'properties' or '$defs', got: {schema}",
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let input = serde_json::json!({
            "operation": "write",
            "path": "../outside.txt",
            "content": "bad"
        });
        let err = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap_err();
        assert!(matches!(err, ToolError::VirtualFsConflict { .. }));
    }

    #[tokio::test]
    #[allow(unused_variables)]
    async fn rejects_absolute_path_outside_root() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        // Construct an absolute path outside the temp dir
        let outside_path = std::env::temp_dir().join("outside.txt");
        let input = serde_json::json!({
            "operation": "write",
            "path": outside_path.to_str().unwrap(),
            "content": "bad"
        });
        let err = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap_err();
        assert!(matches!(err, ToolError::VirtualFsConflict { .. }));
    }

    #[tokio::test]
    async fn write_read_delete_reject_reserved_device_names() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let session = session_for(dir.path());

        for name in ["nul", "NUL", "Con", "com1"] {
            let err = tool
                .execute(
                    serde_json::json!({"operation": "write", "path": name, "content": "x"}),
                    &policy,
                    &session,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::ExecutionFailed { .. }));
            let text = err.to_string();
            assert!(
                text.contains("reserved Windows device name")
                    && text.to_ascii_lowercase().contains(&name.to_ascii_lowercase()),
                "write '{name}' must be rejected with the reserved-name message, got: {text}"
            );

            let err = tool
                .execute(
                    serde_json::json!({"operation": "read", "path": name}),
                    &policy,
                    &session,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("reserved Windows device name"),
                "read '{name}' must be rejected, got: {err}"
            );

            let err = tool
                .execute(
                    serde_json::json!({"operation": "delete", "path": name}),
                    &policy,
                    &session,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("reserved Windows device name"),
                "delete '{name}' must be rejected, got: {err}"
            );
        }

        // No device-named file was ever materialized in the workspace.
        assert!(!dir.path().join("nul").exists());
        assert!(!dir.path().join("Con").exists());
    }

    #[tokio::test]
    async fn device_lookalike_names_work_as_ordinary_files() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let session = session_for(dir.path());

        for name in ["nul.txt", "com10"] {
            tool.execute(
                serde_json::json!({"operation": "write", "path": name, "content": "x"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("writing an ordinary lookalike file must work");
        }
        assert!(dir.path().join("nul.txt").is_file());
        assert!(dir.path().join("com10").is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        // Create a file outside the root
        let outside_file = std::env::temp_dir().join("outside.txt");
        std::fs::write(&outside_file, "data").unwrap();
        // Create symlink inside root pointing to outside file
        let link_path = dir.path().join("link.txt");
        symlink(&outside_file, &link_path).unwrap();
        let input = serde_json::json!({
            "operation": "read",
            "path": "link.txt"
        });
        let err = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap_err();
        assert!(matches!(err, ToolError::VirtualFsConflict { .. }));
    }

    #[tokio::test]
    async fn move_operation_works() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let session = session_for(dir.path());

        // First write a file
        tool.execute(
            serde_json::json!({"operation": "write", "path": "source.txt", "content": "move me"}),
            &policy,
            &session,
            cancel.clone(),
        )
        .await
        .unwrap();

        // Move it
        let output = tool.execute(
            serde_json::json!({"operation": "move", "path": "source.txt", "destination": "dest.txt"}),
            &policy, &session, cancel.clone(),
        ).await.unwrap();
        assert!(
            output.summary.contains("Moved"),
            "expected move confirmation, got: {}",
            output.summary
        );

        // Verify source no longer exists
        let read_src = tool
            .execute(
                serde_json::json!({"operation": "exists", "path": "source.txt"}),
                &policy,
                &session,
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(!read_src.data["exists"].as_bool().unwrap());

        // Verify dest exists with content
        let read_dest = tool
            .execute(
                serde_json::json!({"operation": "read", "path": "dest.txt"}),
                &policy,
                &session,
                cancel,
            )
            .await
            .unwrap();
        assert_eq!(read_dest.data["content"], "move me");
    }

    #[tokio::test]
    async fn copy_operation_works() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let session = session_for(dir.path());

        tool.execute(
            serde_json::json!({"operation": "write", "path": "original.txt", "content": "copy me"}),
            &policy,
            &session,
            cancel.clone(),
        )
        .await
        .unwrap();

        let output = tool.execute(
            serde_json::json!({"operation": "copy", "path": "original.txt", "destination": "copy.txt"}),
            &policy, &session, cancel.clone(),
        ).await.unwrap();
        assert!(output.summary.contains("Copied"));

        // Both files should exist
        let original = tool
            .execute(
                serde_json::json!({"operation": "read", "path": "original.txt"}),
                &policy,
                &session,
                cancel.clone(),
            )
            .await
            .unwrap();
        let copy = tool
            .execute(
                serde_json::json!({"operation": "read", "path": "copy.txt"}),
                &policy,
                &session,
                cancel,
            )
            .await
            .unwrap();
        assert_eq!(original.data["content"], "copy me");
        assert_eq!(copy.data["content"], "copy me");
    }

    #[tokio::test]
    async fn write_rejects_empty_content() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let input = serde_json::json!({
            "operation": "write",
            "path": "empty.txt",
            "content": ""
        });
        // Should succeed (empty files are valid)
        let result = tool.execute(input, &policy, &session_for(dir.path()), cancel).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn write_normalizes_typographic_characters_on_disk() {
        let (tool, dir) = tool_and_dir();
        // Curly double quotes (U+201C/U+201D), non-breaking hyphen (U+2011),
        // en dash (U+2013), rightwards arrow (U+2192), and curly single quotes
        // (U+2018/U+2019) — exactly the characters a model can inject into
        // source files.
        let content = "// \u{201C}heading\u{201D}\nlet s = \u{201C}\u{2011}fast\u{201D};\nlet r = a\u{2013}b;\nlet f = |x| x \u{2192} x;\nlet q = \u{2018}y\u{2019};\n";
        tool.execute(
            serde_json::json!({
                "operation": "write",
                "path": "src/example.rs",
                "content": content,
            }),
            &test_policy(),
            &session_for(dir.path()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let on_disk = std::fs::read_to_string(dir.path().join("src/example.rs")).unwrap();
        let expected =
            "// \"heading\"\nlet s = \"-fast\";\nlet r = a-b;\nlet f = |x| x -> x;\nlet q = 'y';\n";
        assert_eq!(on_disk, expected);
        // No typographic character may survive on disk in any form.
        assert!(
            !on_disk.chars().any(|ch| matches!(
                ch,
                '\u{00A0}'
                    | '\u{202F}'
                    | '\u{2018}'
                    | '\u{2019}'
                    | '\u{201C}'
                    | '\u{201D}'
                    | '\u{2010}'
                    | '\u{2011}'
                    | '\u{2012}'
                    | '\u{2013}'
                    | '\u{2192}'
            )),
            "typographic characters leaked to disk: {on_disk:?}"
        );
    }

    #[tokio::test]
    async fn write_leaves_plain_content_unchanged() {
        let (tool, dir) = tool_and_dir();
        // ASCII-only content (a Cargo.toml) must round-trip byte-identical:
        // the normalizer's borrow fast path must not rewrite anything.
        let content = "[package]\nname = \"example\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde = \"1\"\n";
        tool.execute(
            serde_json::json!({
                "operation": "write",
                "path": "Cargo.toml",
                "content": content,
            }),
            &test_policy(),
            &session_for(dir.path()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap(), content);
    }

    #[tokio::test]
    async fn write_preserves_em_dash() {
        let (tool, dir) = tool_and_dir();
        // U+2014 EM DASH is legal punctuation in prose and comments and is
        // deliberately left untouched by the normalizer contract.
        let content = "// README \u{2014} describes the design\n";
        tool.execute(
            serde_json::json!({
                "operation": "write",
                "path": "notes.txt",
                "content": content,
            }),
            &test_policy(),
            &session_for(dir.path()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(), content);
    }

    #[tokio::test]
    async fn move_requires_destination() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let input = serde_json::json!({
            "operation": "move",
            "path": "source.txt"
        });
        let err = tool.execute(input, &policy, &session_for(dir.path()), cancel).await.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }

    #[test]
    fn coerces_number_typed_path_to_string() {
        let input = serde_json::json!({"operation": "read", "path": 42});
        let parsed = coerce_filesystem_input(&input).unwrap();
        assert_eq!(parsed.operation, "read");
        assert_eq!(parsed.path, "42");
    }

    #[test]
    fn coerces_scalar_content_to_string() {
        let input = serde_json::json!({"operation": "write", "path": "a.txt", "content": 5});
        let parsed = coerce_filesystem_input(&input).unwrap();
        assert_eq!(parsed.content, Some("5".to_string()));
    }

    #[test]
    fn rejects_object_content_with_original_error() {
        let input = serde_json::json!({
            "operation": "write",
            "path": "a.txt",
            "content": {"nested": "map"}
        });
        let err = coerce_filesystem_input(&input).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("invalid filesystem input: "), "unexpected message: {message}");
    }

    #[test]
    fn valid_input_deserializes_unchanged() {
        let input = serde_json::json!({
            "operation": "read",
            "path": "Cargo.toml",
            "content": null,
            "destination": null
        });
        let parsed = coerce_filesystem_input(&input).unwrap();
        assert_eq!(parsed.operation, "read");
        assert_eq!(parsed.path, "Cargo.toml");
        assert_eq!(parsed.content, None);
        assert_eq!(parsed.destination, None);
    }
}
