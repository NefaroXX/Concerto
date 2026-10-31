//! Filesystem tool — exposes `VirtualFs` operations through the `Tool` trait,
//! policy-gated via the executor choke-point.

use crate::virtual_fs::{reject_reserved_device_name, VirtualFs, VirtualFsEntry};
use async_trait::async_trait;
use camino::Utf8Path;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, PathPolicyFacts, SessionContext, ToolOutput};
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
    /// UTF-8 content to write verbatim (required for the "write" operation only).
    #[serde(default)]
    #[schemars(description = "UTF-8 content to write verbatim (required for write operation).")]
    pub content: Option<String>,
    /// Standard-base64-encoded bytes to write (alternative to `content` for
    /// the "write" operation only).
    ///
    /// The byte-replay channel: checkpoint restore replays banked bytes
    /// verbatim through this field because JSON strings are Unicode and
    /// cannot carry arbitrary bytes. Exactly one of `content`/`content_b64`
    /// must be present on a write. Both channels preserve their supplied
    /// content exactly; `content_b64` also supports non-UTF-8 bytes.
    #[serde(default)]
    #[schemars(
        description = "Base64-encoded bytes to write (alternative to content for write operation; written verbatim without normalization)."
    )]
    pub content_b64: Option<String>,
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

// ---------------------------------------------------------------------------
// Guard heuristic inference (adaptive tool-guard Solution 3)
// ---------------------------------------------------------------------------

/// Alias keys weak models emit for the canonical `path` field.
const PATH_ALIASES: [&str; 4] = ["file", "filepath", "file_path", "filename"];

/// Alias keys weak models emit for the canonical `operation` field.
const OPERATION_ALIASES: [&str; 2] = ["op", "action"];

/// Conservative heuristic inference for missing required filesystem arguments
/// (adaptive tool-guard Solution 3: last-mile adaptability for weak
/// tool-calling models).
///
/// Called by the orchestrator's tool-call guard only when a required field is
/// absent or `null` after parse+coerce. `raw` is the model's ORIGINAL argument
/// object (pre-coercion, so hallucinated alias keys are still present) and
/// `missing` lists the unresolved required field names. Returns `(field,
/// value)` insertions for the guard to apply; the guard re-coerces and
/// re-validates the completed arguments, so a wrong guess can never reach the
/// executor — it degrades to the usual corrective reject.
///
/// Rules, most-specific first (direct evidence beats inference, and `path` is
/// never invented from nothing):
///
/// 1. `operation` ← `op`/`action` alias (non-empty string values only).
/// 2. `operation` ← `write` when a non-null string `content` is present
///    (content is only meaningful for the write operation).
/// 3. `operation` ← `list` when the path is directory-like (`.` or a trailing
///    separator), else `read` — the least destructive default, whose failure
///    mode is the tool's own corrective "use list for directories" error.
/// 4. `path` ← `file`/`filepath`/`file_path`/`filename` alias (non-empty
///    string values only).
pub fn infer_missing_arguments(
    raw: &serde_json::Map<String, serde_json::Value>,
    missing: &[String],
) -> Vec<(String, serde_json::Value)> {
    let mut inferred = Vec::new();

    if missing.iter().any(|field| field == "operation") {
        if let Some(operation) = infer_operation(raw) {
            inferred.push(("operation".to_string(), serde_json::Value::String(operation)));
        }
    }

    if missing.iter().any(|field| field == "path") {
        if let Some(path) = non_empty_string_alias(raw, &PATH_ALIASES) {
            inferred.push(("path".to_string(), serde_json::Value::String(path)));
        }
    }

    inferred
}

/// Operation inference for one raw argument object, per the documented rules.
fn infer_operation(raw: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(alias) = non_empty_string_alias(raw, &OPERATION_ALIASES) {
        return Some(alias);
    }
    if raw.get("content").is_some_and(serde_json::Value::is_string) {
        return Some("write".to_string());
    }
    match raw.get("path").and_then(serde_json::Value::as_str) {
        Some(path) => Some(operation_for_path(path).to_string()),
        None => {
            non_empty_string_alias(raw, &PATH_ALIASES).map(|path| operation_for_path(&path).into())
        }
    }
}

/// Path-shape operation: directories list, everything else reads.
fn operation_for_path(path: &str) -> &'static str {
    if is_directory_like(path) {
        "list"
    } else {
        "read"
    }
}

/// Directory-shape test for `list` inference: the workspace root or a path
/// explicitly terminated as a directory. Both separators are accepted because
/// models on Windows-style paths still mean a directory.
fn is_directory_like(path: &str) -> bool {
    let trimmed = path.trim();
    trimmed == "." || trimmed.ends_with('/') || trimmed.ends_with('\\')
}

/// First non-empty (after trimming) string value among `keys`.
///
/// Whitespace-only values carry no signal and are skipped; non-string values
/// are never guessed at by this layer.
fn non_empty_string_alias(
    raw: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        raw.get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
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

    /// Byte-verbatim write for the `content_b64` channel (see the write arm).
    ///
    /// Parents are materialized, `bytes` land on disk exactly, and the VFS
    /// stages them via [`VirtualFs::write_bytes`] — no typographic
    /// normalization, so checkpoint byte-restore replays banked bytes
    /// exactly. An unstaged on-disk file is loaded first (like the text
    /// path) so an existing file stages `ModifiedBytes` rather than
    /// `CreatedBytes`. H02: OS failures are typed `Io`, mirroring the text
    /// path's classification exactly.
    fn execute_bytes_write(
        path: &Utf8Path,
        path_str: &str,
        root: &Utf8Path,
        vfs: &mut VirtualFs,
        bytes: Vec<u8>,
    ) -> Result<ToolOutput, ToolError> {
        // Capture the original contents before materializing the write
        // (same classification rule as the text path).
        if !vfs.exists(path) && path.as_std_path().is_file() {
            vfs.read_disk(path)?;
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent.as_std_path()).map_err(|e| {
                ToolError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "failed to create parent directory for '{path_str}' \
                         in workspace '{root}': {e}"
                    ),
                ))
            })?;
        }

        let content_len = bytes.len();
        std::fs::write(path.as_std_path(), bytes.as_slice()).map_err(|e| {
            ToolError::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "failed to write '{path_str}' at '{path}' in workspace '{root}': \
                     {e}. Check that the selected project folder is writable and is not \
                     blocked by Windows Controlled Folder Access"
                ),
            ))
        })?;

        vfs.write_bytes(path, bytes)?;

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
}

/// Return `ToolError::Cancelled` if the token has been cancelled.
fn check_cancel(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

/// The root path used for containment, mirroring [`FilesystemTool::execute`]:
/// the session's project dir when set, otherwise the tool's construction root.
fn effective_root<'a>(tool_root: &'a Utf8Path, session: &'a SessionContext) -> &'a Utf8Path {
    if !session.project_dir.as_os_str().is_empty() {
        session.project_dir.to_str().map(Utf8Path::new).unwrap_or(tool_root)
    } else {
        tool_root
    }
}

/// Build structured path facts for a filesystem action from its raw input.
///
/// Records the operation, the attempted path exactly as the caller supplied it,
/// and the confined absolute path after workspace containment (recorded
/// separately so a containment rewrite is visible). Move/copy also record the
/// attempted and confined destination. Never records file content — only the
/// operation and paths.
///
/// Returns `None` only when the input cannot be parsed as a filesystem action;
/// a path that fails containment still yields facts with `resolved_path: None`
/// so a rejected traversal remains attributable.
fn filesystem_path_facts(root: &Utf8Path, input: &serde_json::Value) -> Option<PathPolicyFacts> {
    let parsed = coerce_filesystem_input(input).ok()?;
    let resolved_path = crate::common::resolve_path(root, Utf8Path::new(&parsed.path))
        .ok()
        .map(|path| path.to_string());
    let (attempted_destination, resolved_destination) = match parsed.destination.as_deref() {
        Some(destination) if !destination.is_empty() => (
            Some(destination.to_string()),
            crate::common::resolve_path(root, Utf8Path::new(destination))
                .ok()
                .map(|path| path.to_string()),
        ),
        _ => (None, None),
    };
    Some(PathPolicyFacts {
        operation: parsed.operation,
        attempted_path: Some(parsed.path),
        resolved_path,
        attempted_destination,
        resolved_destination,
    })
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

    fn path_facts(
        &self,
        input: &serde_json::Value,
        session: &SessionContext,
    ) -> Option<PathPolicyFacts> {
        let root = effective_root(&self.root, session);
        filesystem_path_facts(root, input)
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
                // Pending changes (including deletion) win over disk. Clean
                // originals are observations, so refresh them on every read.
                let content = if let Some(max_bytes) = input.get("max_bytes").and_then(serde_json::Value::as_u64) {
                    let content = vfs.read_bounded(&path, usize::try_from(max_bytes).unwrap_or(usize::MAX))?;
                    // Keep an existing observation aligned with the returned
                    // bytes, just as read_disk does for unbounded reads.
                    if matches!(vfs.get(&path), Some(VirtualFsEntry::Original { .. })) {
                        vfs.insert(VirtualFsEntry::Original {
                            path: path.clone(), content: content.clone(),
                        });
                    }
                    content
                } else if !matches!(vfs.get(&path), None | Some(VirtualFsEntry::Original { .. })) {
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
                // Exactly one of the byte (`content_b64`) or UTF-8 text
                // (`content`) channels must be present. Both are verbatim.
                let replay_bytes: Option<Vec<u8>> = match (
                    fs_input.content.as_deref(),
                    fs_input.content_b64.as_deref(),
                ) {
                    (Some(_), Some(_)) => {
                        return Err(ToolError::ExecutionFailed {
                            message: "write accepts exactly one of 'content' or 'content_b64'"
                                .into(),
                        });
                    }
                    (None, Some(b64)) => {
                        use base64::Engine;
                        Some(
                            base64::engine::general_purpose::STANDARD.decode(b64).map_err(
                                |error| ToolError::ExecutionFailed {
                                    message: format!("invalid 'content_b64': {error}"),
                                },
                            )?,
                        )
                    }
                    (Some(_), None) => None,
                    (None, None) => {
                        return Err(ToolError::ExecutionFailed {
                            message: "missing 'content' field for write operation".into(),
                        });
                    }
                };
                if let Some(bytes) = replay_bytes {
                    return Self::execute_bytes_write(&path, path_str, root, &mut vfs, bytes);
                }
                let content =
                    fs_input.content.ok_or_else(|| ToolError::ExecutionFailed {
                        message: "missing 'content' field for write operation".into(),
                    })?;
                // File contents are opaque data. Even punctuation that looks
                // like a mistaken delimiter may be intentional inside a string.
                let content_len = content.len();

                // Capture the original contents before materializing the write.
                // Otherwise an existing file is misclassified as Created and
                // rejecting the change can delete the user's original file.
                if !vfs.exists(&path) && path.as_std_path().is_file() {
                    vfs.read_disk(&path)?;
                }

                if let Some(parent) = path.parent() {
                    // H02: parent-dir creation is an OS verdict — typed `Io`
                    // (`NotFound`/`PermissionDenied`/… classify
                    // structurally). The pre-typing context (path +
                    // workspace) rides the io payload; kind preserved
                    // exactly, so classification is identical to the
                    // bare-kind conversion.
                    std::fs::create_dir_all(parent.as_std_path()).map_err(|e| {
                        ToolError::Io(std::io::Error::new(
                            e.kind(),
                            format!(
                                "failed to create parent directory for '{path_str}' \
                                 in workspace '{root}': {e}"
                            ),
                        ))
                    })?;
                }

                std::fs::write(path.as_std_path(), content.as_bytes()).map_err(|e| {
                    ToolError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "failed to write '{path_str}' at '{path}' in workspace '{root}': \
                             {e}. Check that the selected project folder is writable and is not \
                             blocked by Windows Controlled Folder Access"
                        ),
                    ))
                })?;

                vfs.write(&path, content)?;

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
                    // H02: disk delete is an OS verdict — typed `Io`. The
                    // deleted path rides the io payload, restored from the
                    // pre-typing message; kind preserved exactly.
                    std::fs::remove_file(path.as_std_path()).map_err(|e| {
                        ToolError::Io(std::io::Error::new(
                            e.kind(),
                            format!("failed to delete {path_str} from disk: {e}"),
                        ))
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
                let exists = if vfs.get(&path).is_some() {
                    vfs.exists(&path)
                } else {
                    vfs.exists_on_disk(&path)
                };
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
                // H02: directory listing is an OS verdict — typed `Io`.
                // The listed path + workspace ride the io payload, restored
                // from the pre-typing message; kind preserved exactly.
                let mut entries = std::fs::read_dir(path.as_std_path())
                    .map_err(|error| {
                        ToolError::Io(std::io::Error::new(
                            error.kind(),
                            format!(
                                "failed to list '{path_str}' in workspace '{root}': {error}"
                            ),
                        ))
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
                // Also perform the real filesystem rename.
                // H02: typed `Io`. The source/destination ride the io
                // payload, restored from the pre-typing message; kind
                // preserved exactly.
                if path.as_std_path().exists() {
                    std::fs::rename(path.as_std_path(), dest_path.as_std_path()).map_err(|e| {
                        ToolError::Io(std::io::Error::new(
                            e.kind(),
                            format!("failed to move '{path_str}' to '{dest}': {e}"),
                        ))
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
                // Also perform the real filesystem copy.
                // H02: typed `Io`. The source/destination ride the io
                // payload, restored from the pre-typing message; kind
                // preserved exactly.
                if path.as_std_path().exists() {
                    std::fs::copy(path.as_std_path(), dest_path.as_std_path()).map_err(|e| {
                        ToolError::Io(std::io::Error::new(
                            e.kind(),
                            format!("failed to copy '{path_str}' to '{dest}': {e}"),
                        ))
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

// ---------------------------------------------------------------------------
// `write` alias tool (Claude-habit compatibility)
// ---------------------------------------------------------------------------

/// Input schema for the thin [`WriteTool`] alias.
///
/// `path` and `content` are both required: the alias has exactly one job, so
/// there is no optional `operation`/`destination` surface to guess at. The
/// orchestrator's tool-call guard validates against this schema and coaches a
/// model that omits either field.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteInput {
    /// Path relative to project root.
    #[schemars(description = "Path of the file to write, relative to project root.")]
    pub path: String,
    /// Full content to write to the file.
    #[schemars(description = "Content to write to the file.")]
    pub content: String,
}

/// Thin `write` alias forwarding to the filesystem tool's `write` operation.
///
/// Models trained on Claude-style tool names habitually call a bare `write`
/// tool. Rather than making them recover through
/// `filesystem(operation=write)` on every call, this alias presents their
/// expected shape (`{path, content}`) and delegates to the canonical
/// filesystem write path.
///
/// Policy parity is structural, not duplicated: [`Tool::policy_view`] reports
/// the canonical `filesystem` tool name together with an input carrying
/// `operation: "write"`, so every `Condition::ToolName("filesystem")` and
/// `Condition::Operation("write")` rule (and the catch-all) matches exactly as
/// it would for a direct `filesystem` write. Execution still runs through the
/// shared `VirtualFs` and returns the filesystem tool's own output.
pub struct WriteTool {
    inner: FilesystemTool,
}

impl WriteTool {
    /// Creates a `write` alias rooted at `root` with its own `VirtualFs`.
    pub fn new(root: camino::Utf8PathBuf) -> Self {
        Self { inner: FilesystemTool::new(root) }
    }

    /// Creates a `write` alias sharing an existing `VirtualFs` with the
    /// canonical filesystem tool, so both observe the same overlay.
    pub fn new_shared(root: camino::Utf8PathBuf, vfs: Arc<Mutex<VirtualFs>>) -> Self {
        Self { inner: FilesystemTool::new_shared(root, vfs) }
    }

    /// The canonical filesystem-write input for a raw alias input: the raw
    /// object with `operation` forced to `write`.
    fn canonical_input(input: &serde_json::Value) -> serde_json::Value {
        let mut object = input.as_object().cloned().unwrap_or_default();
        object.insert("operation".to_string(), serde_json::Value::String("write".to_string()));
        serde_json::Value::Object(object)
    }
}

#[async_trait]
impl concerto_core::traits::tool::Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file within the project workspace. Alias for the filesystem write operation."
    }

    fn input_schema(&self) -> serde_json::Value {
        // The alias always writes, so the advertised contract is just the
        // write shape: `path` and `content` required.
        let root = schemars::schema_for!(WriteInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize WriteInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path of the file to write, relative to project root." },
                    "content": { "type": "string", "description": "Content to write to the file." }
                },
                "required": ["path", "content"]
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // Same coarse flag as the canonical filesystem tool: the write itself
        // is enforced by policy, never by this filter.
        self.inner.capability_requirements()
    }

    fn policy_view(&self, input: &serde_json::Value) -> (String, serde_json::Value) {
        // Canonical identity + operation-bearing input: full parity with a
        // direct filesystem write under every rule shape.
        ("filesystem".to_string(), Self::canonical_input(input))
    }

    fn path_facts(
        &self,
        input: &serde_json::Value,
        session: &SessionContext,
    ) -> Option<PathPolicyFacts> {
        // The executor passes the canonical policy view (operation forced to
        // `write`), so the shared filesystem fact builder sees the operation.
        let root = effective_root(&self.inner.root, session);
        filesystem_path_facts(root, input)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.inner.execute(Self::canonical_input(&input), policy, session, cancel).await
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
    async fn freshness_tool_reads_follow_external_changes_and_deletion() {
        let (tool, dir) = tool_and_dir();
        let path = dir.path().join("live.txt");
        std::fs::write(&path, "before").unwrap();
        let resolved = crate::common::resolve_path(&tool.root, Utf8Path::new("live.txt")).unwrap();
        for (expected, limit) in [("before", None), ("after!", None), ("bounded", Some(7))] {
            std::fs::write(&path, expected).unwrap();
            let mut input = serde_json::json!({"operation": "read", "path": "live.txt"});
            if let Some(limit) = limit {
                input["max_bytes"] = serde_json::json!(limit);
            }
            let output = tool
                .execute(input, &test_policy(), &session_for(dir.path()), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(output.data["content"], expected);
            assert_eq!(
                tool.vfs().lock().unwrap().get(&resolved).unwrap().original_content(),
                Some(expected),
            );
        }
        std::fs::remove_file(&path).unwrap();
        let result = tool
            .execute(
                serde_json::json!({"operation": "read", "path": "live.txt"}),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
        let exists = tool
            .execute(
                serde_json::json!({"operation": "exists", "path": "live.txt"}),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(exists.data["exists"], false);
    }

    #[tokio::test]
    async fn freshness_tool_does_not_resurrect_a_staged_deletion() {
        let (tool, dir) = tool_and_dir();
        let path = dir.path().join("deleted.txt");
        std::fs::write(&path, "still on disk").unwrap();
        let resolved =
            crate::common::resolve_path(&tool.root, Utf8Path::new("deleted.txt")).unwrap();
        tool.vfs().lock().unwrap().stage_delete(&resolved).unwrap();
        for input in [
            serde_json::json!({"operation": "read", "path": "deleted.txt"}),
            serde_json::json!({"operation": "read", "path": "deleted.txt", "max_bytes": 100}),
        ] {
            assert!(tool
                .execute(input, &test_policy(), &session_for(dir.path()), CancellationToken::new())
                .await
                .is_err());
        }
        let exists = tool
            .execute(
                serde_json::json!({"operation": "exists", "path": "deleted.txt"}),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(exists.data["exists"], false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "still on disk");
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
        // Canonical temp root, so both sides of the VFS lookup are canonical:
        // `resolve_path` keys entries by the canonical path it returns, while
        // `TMPDIR` on macOS is a symlinked `/var/folders` → `/private/var/folders`.
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("utf8 temp path")
            .canonicalize_utf8()
            .expect("canonical temp root");
        let path = root.join("existing.txt");
        std::fs::write(&path, "original").unwrap();

        tool.execute(
            serde_json::json!({
                "operation": "write",
                "path": "existing.txt",
                "content": "changed"
            }),
            &test_policy(),
            &session_for(root.as_std_path()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed");

        let utf8_path = path.clone();
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

    /// H02 construction-site typing: a write whose parent directory cannot
    /// be created (a regular file blocks the path) surfaces as the
    /// structural `ToolError::Io` — `is_deterministic_failure` holds by
    /// `ErrorKind`, and the envelope derives `exec.io_error` /
    /// `do-not-re-issue` without reading any message.
    #[tokio::test]
    async fn write_beneath_a_regular_file_yields_typed_io() {
        let (tool, dir) = tool_and_dir();
        std::fs::write(dir.path().join("blocker"), "x").unwrap();
        let error = tool
            .execute(
                serde_json::json!({
                    "operation": "write",
                    "path": "blocker/child.txt",
                    "content": "y"
                }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        match &error {
            ToolError::Io(io) => assert!(
                matches!(
                    io.kind(),
                    std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::AlreadyExists
                ),
                "the OS verdict is preserved, got {:?}",
                io.kind()
            ),
            other => panic!("expected typed ToolError::Io, got {other:?}"),
        }
        // Context restoration (H02 follow-up): the pre-typing operator
        // context (target path + workspace) rides the io payload's Display.
        let display = error.to_string();
        assert!(
            display.contains("failed to create parent directory"),
            "missing op context: {display}"
        );
        assert!(display.contains("blocker/child.txt"), "missing path: {display}");
        assert!(display.contains("in workspace"), "missing workspace: {display}");
        assert!(error.is_deterministic_failure(), "the kind classifies structurally");
        let failure = concerto_core::tool_failure::ToolFailure::from_error(
            &error,
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert_eq!(failure.code, "exec.io_error", "the envelope is typed: {failure:?}");
        assert_eq!(
            failure.disposition,
            concerto_core::tool_failure::ToolDisposition::DoNotReIssue,
            "the disposition derives from the kind"
        );
        assert!(!failure.opaque, "a builtin failure is never opaque");
        // Context presence never flips classification: same kind, bare OS
        // text only → identical ledger + envelope.
        let kind = match &error {
            ToolError::Io(io) => io.kind(),
            other => panic!("expected typed ToolError::Io, got {other:?}"),
        };
        let bare = ToolError::Io(std::io::Error::new(kind, "os verdict"));
        assert_eq!(
            error.is_deterministic_failure(),
            bare.is_deterministic_failure(),
            "classification is kind-for-kind, context text is inert"
        );
        let bare_failure = concerto_core::tool_failure::ToolFailure::from_error(
            &bare,
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert_eq!(failure.code, bare_failure.code, "same envelope code");
        assert_eq!(
            failure.disposition, bare_failure.disposition,
            "same disposition with and without context text"
        );
    }

    /// H02 construction-site typing: a write targeting an existing directory
    /// (the `std::fs::write` site) surfaces as the structural `ToolError::Io`
    /// — classified by `ErrorKind` — and the pre-typing context (path +
    /// workspace + writability/CFA coaching) rides the io payload. Context
    /// presence never flips classification.
    #[tokio::test]
    async fn write_into_a_directory_path_yields_typed_io_with_context() {
        let (tool, dir) = tool_and_dir();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let error = tool
            .execute(
                serde_json::json!({
                    "operation": "write",
                    "path": "subdir",
                    "content": "y"
                }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let kind = match &error {
            ToolError::Io(io) => {
                let kind = io.kind();
                assert!(
                    matches!(
                        kind,
                        std::io::ErrorKind::IsADirectory | std::io::ErrorKind::PermissionDenied
                    ),
                    "the OS verdict is preserved, got {kind:?}"
                );
                kind
            }
            other => panic!("expected typed ToolError::Io, got {other:?}"),
        };
        let display = error.to_string();
        assert!(display.contains("failed to write"), "missing op context: {display}");
        assert!(display.contains("subdir"), "missing path: {display}");
        assert!(display.contains("in workspace"), "missing workspace: {display}");
        assert!(
            display.contains("Windows Controlled Folder Access"),
            "missing writability coaching: {display}"
        );
        assert!(error.is_deterministic_failure(), "the kind classifies structurally");
        let failure = concerto_core::tool_failure::ToolFailure::from_error(
            &error,
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert_eq!(failure.code, "exec.io_error", "the envelope is typed: {failure:?}");
        assert_eq!(
            failure.disposition,
            concerto_core::tool_failure::ToolDisposition::DoNotReIssue,
            "the disposition derives from the kind"
        );
        let bare = ToolError::Io(std::io::Error::new(kind, "os verdict"));
        assert_eq!(
            error.is_deterministic_failure(),
            bare.is_deterministic_failure(),
            "classification is kind-for-kind, context text is inert"
        );
        let bare_failure = concerto_core::tool_failure::ToolFailure::from_error(
            &bare,
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert_eq!(failure.code, bare_failure.code, "same envelope code");
        assert_eq!(
            failure.disposition, bare_failure.disposition,
            "same disposition with and without context text"
        );
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

    // -----------------------------------------------------------------------
    // Reproduction — an existing in-root file that was never staged in the
    // overlay must still move/copy (ADR-62 §4: reads resolve through the
    // overlay onto disk). Observed on Windows: a file created on disk before
    // the session made `move` return "file not found" in 0-1 ms — an early
    // bail from the overlay-only source lookup, not a filesystem check.
    // -----------------------------------------------------------------------

    /// The reproducer: the file exists on disk but was never read or written
    /// through the tool's `VirtualFs`, so it has no overlay entry. Before the
    /// fix the move failed with "file not found"; after it must succeed and
    /// rename on disk.
    #[tokio::test]
    async fn move_finds_existing_disk_file_absent_from_overlay() {
        let (tool, dir) = tool_and_dir();
        // Created on disk directly — exactly the "created on disk in the
        // project dir" precondition (the overlay never saw it).
        std::fs::write(dir.path().join("file_test.txt"), "").unwrap();
        assert!(dir.path().join("file_test.txt").exists());

        let output = tool
            .execute(
                serde_json::json!({
                    "operation": "move",
                    "path": "file_test.txt",
                    "destination": "file_test_pass.txt"
                }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .expect("an existing in-root file must be movable even when absent from the overlay");
        assert!(output.summary.contains("Moved"), "unexpected summary: {}", output.summary);
        assert!(!dir.path().join("file_test.txt").exists(), "source must be gone after move");
        assert!(dir.path().join("file_test_pass.txt").exists(), "destination must exist");
    }

    /// The `list` and `exists` operations already fall through to disk; prove
    /// the directory is enumerable while the file is present, matching the
    /// Windows evidence (`list .` succeeded seconds before `move` failed).
    #[tokio::test]
    async fn disk_file_is_listable_and_existing_before_move() {
        let (tool, dir) = tool_and_dir();
        std::fs::write(dir.path().join("file_test.txt"), "").unwrap();
        let session = session_for(dir.path());

        let listed = tool
            .execute(
                serde_json::json!({"operation": "list", "path": "."}),
                &test_policy(),
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("listing the workspace root must succeed");
        assert!(
            listed.data["entries"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|e| e["name"] == "file_test.txt")),
            "the on-disk file must be listed: {}",
            listed.data
        );

        let exists = tool
            .execute(
                serde_json::json!({"operation": "exists", "path": "file_test.txt"}),
                &test_policy(),
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("exists must succeed");
        assert!(exists.data["exists"].as_bool().unwrap(), "the on-disk file must exist");
    }

    /// Fail-closed: when the source is genuinely absent (overlay and disk),
    /// the move still reports "file not found".
    #[tokio::test]
    async fn move_missing_source_still_reports_not_found() {
        let (tool, dir) = tool_and_dir();
        let error = tool
            .execute(
                serde_json::json!({
                    "operation": "move",
                    "path": "ghost.txt",
                    "destination": "ghost_pass.txt"
                }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .expect_err("a missing source must not move");
        assert!(
            error.to_string().contains("file not found"),
            "expected a not-found error, got: {error}"
        );
        assert!(!dir.path().join("ghost_pass.txt").exists(), "no destination may be created");
    }

    /// A staged deletion must not be resurrected from disk by the move fix.
    #[tokio::test]
    async fn move_does_not_resurrect_staged_deletion() {
        let (tool, dir) = tool_and_dir();
        std::fs::write(dir.path().join("doomed.txt"), "bye").unwrap();
        let session = session_for(dir.path());

        // Stage a delete (this loads the disk file as Original then marks it
        // Deleted in the overlay).
        tool.execute(
            serde_json::json!({"operation": "delete", "path": "doomed.txt"}),
            &test_policy(),
            &session,
            CancellationToken::new(),
        )
        .await
        .expect("delete of an on-disk file must succeed");

        let error = tool
            .execute(
                serde_json::json!({
                    "operation": "move",
                    "path": "doomed.txt",
                    "destination": "survivor.txt"
                }),
                &test_policy(),
                &session,
                CancellationToken::new(),
            )
            .await
            .expect_err("a staged deletion must not be moved");
        assert!(
            error.to_string().contains("deleted"),
            "expected a deleted-source error, got: {error}"
        );
    }

    #[tokio::test]
    async fn copy_finds_existing_disk_file_absent_from_overlay() {
        let (tool, dir) = tool_and_dir();
        std::fs::write(dir.path().join("file_test.txt"), "contents").unwrap();

        let output = tool
            .execute(
                serde_json::json!({
                    "operation": "copy",
                    "path": "file_test.txt",
                    "destination": "file_test_copy.txt"
                }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .expect("an existing in-root file must be copyable even when absent from the overlay");
        assert!(output.summary.contains("Copied"), "unexpected summary: {}", output.summary);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file_test_copy.txt")).unwrap(),
            "contents"
        );
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
    async fn write_b64_round_trips_binary_bytes_verbatim() {
        use base64::Engine as _;
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        // Non-UTF-8 bytes that can never ride the `content` field.
        let bytes = vec![0x00, 0xFF, 0xFE, 0x01, 0x02, b'h', b'i'];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        tool.execute(
            serde_json::json!({"operation": "write", "path": "bin.dat", "content_b64": b64}),
            &policy,
            &session_for(dir.path()),
            CancellationToken::new(),
        )
        .await
        .expect("b64 write succeeds");
        // Disk holds the exact bytes (no normalization, no text guessing).
        assert_eq!(std::fs::read(dir.path().join("bin.dat")).unwrap(), bytes);
        // The overlay stages them too: commit re-applies the same bytes.
        let absolute = camino::Utf8PathBuf::from_path_buf(dir.path().join("bin.dat")).unwrap();
        let vfs = tool.vfs().lock().unwrap();
        assert_eq!(vfs.read_bytes(&absolute).unwrap(), bytes);
    }

    #[tokio::test]
    async fn write_b64_rejects_ambiguous_and_invalid_inputs() {
        let (tool, dir) = tool_and_dir();
        let policy = test_policy();
        // Both channels at once: refused (exactly one must be present).
        let both = tool
            .execute(
                serde_json::json!({
                    "operation": "write",
                    "path": "a.bin",
                    "content": "text",
                    "content_b64": "aGk=",
                }),
                &policy,
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await;
        assert!(both.is_err(), "content + content_b64 must be refused");
        // Invalid base64: refused loudly, nothing written.
        let bad = tool
            .execute(
                serde_json::json!({
                    "operation": "write",
                    "path": "b.bin",
                    "content_b64": "!!!not-base64!!!",
                }),
                &policy,
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await;
        assert!(bad.is_err(), "invalid base64 must be refused");
        assert!(!dir.path().join("b.bin").exists(), "refused write writes nothing");
    }

    #[tokio::test]
    async fn write_preserves_unicode_json_and_source_on_disk_and_in_overlay() {
        let (tool, dir) = tool_and_dir();
        // Unicode inside ASCII-delimited strings is valid data. Replacing
        // curly quotes with ASCII quotes would make both fixtures invalid.
        for (relative, content) in [
            ("data.json", "{\"message\":\"‘hi’ “world” ‐‑‒– →\u{00a0}\u{202f}東京\"}\n"),
            (
                "src/example.rs",
                "const MESSAGE: &str = \"‘hi’ “world” ‐‑‒– →\u{00a0}\u{202f}東京\";\n",
            ),
        ] {
            let output = tool
                .execute(
                    serde_json::json!({
                        "operation": "write",
                        "path": relative,
                        "content": content,
                    }),
                    &test_policy(),
                    &session_for(dir.path()),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let path = dir.path().join(relative);
            assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
            assert_eq!(output.data["size"], content.len());
            let vfs = tool.vfs().lock().unwrap();
            assert_eq!(
                vfs.read_bytes(Utf8Path::from_path(&path).unwrap()).unwrap(),
                content.as_bytes()
            );
        }
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("data.json")).unwrap()).unwrap();
        assert_eq!(json["message"], "‘hi’ “world” ‐‑‒– →\u{00a0}\u{202f}東京");
    }

    #[tokio::test]
    async fn write_leaves_plain_content_unchanged() {
        let (tool, dir) = tool_and_dir();
        // ASCII-only content (a Cargo.toml) must also round-trip byte-identical.
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
        // Prose punctuation is preserved just like source and data content.
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

    // -- guard heuristic inference (Solution 3) --------------------------------

    /// Builds the `missing` argument for [`infer_missing_arguments`].
    fn missing(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|field| (*field).to_string()).collect()
    }

    #[test]
    fn infer_read_when_only_path_is_present() {
        let raw = serde_json::json!({ "path": "src/main.rs" }).as_object().unwrap().clone();
        let inferred = infer_missing_arguments(&raw, &missing(&["operation"]));
        assert_eq!(inferred, vec![("operation".to_string(), serde_json::json!("read"))]);
    }

    #[test]
    fn infer_list_for_directory_like_paths() {
        for path in [".", "src/", "src\\"] {
            let raw = serde_json::json!({ "path": path }).as_object().unwrap().clone();
            let inferred = infer_missing_arguments(&raw, &missing(&["operation"]));
            assert_eq!(inferred[0].1, serde_json::json!("list"), "path {path:?}");
        }
    }

    #[test]
    fn infer_write_when_content_is_present() {
        let raw =
            serde_json::json!({ "path": "a.rs", "content": "hi" }).as_object().unwrap().clone();
        let inferred = infer_missing_arguments(&raw, &missing(&["operation"]));
        assert_eq!(inferred[0].1, serde_json::json!("write"));
    }

    #[test]
    fn operation_alias_wins_over_shape_inference() {
        let raw =
            serde_json::json!({ "op": " list ", "path": "a.rs" }).as_object().unwrap().clone();
        let inferred = infer_missing_arguments(&raw, &missing(&["operation"]));
        assert_eq!(inferred[0].1, serde_json::json!("list"), "trimmed alias value wins");
    }

    #[test]
    fn path_alias_is_recovered() {
        for alias in PATH_ALIASES {
            let mut raw = serde_json::Map::new();
            raw.insert(alias.to_string(), serde_json::json!("src/main.rs"));
            let inferred = infer_missing_arguments(&raw, &missing(&["path"]));
            assert_eq!(
                inferred,
                vec![("path".to_string(), serde_json::json!("src/main.rs"))],
                "alias {alias}"
            );
        }
    }

    #[test]
    fn operation_alias_is_recovered() {
        for alias in OPERATION_ALIASES {
            let mut raw = serde_json::Map::new();
            raw.insert(alias.to_string(), serde_json::json!("read"));
            let inferred = infer_missing_arguments(&raw, &missing(&["operation"]));
            assert_eq!(inferred[0].1, serde_json::json!("read"), "alias {alias}");
        }
    }

    #[test]
    fn empty_or_non_string_alias_values_are_ignored() {
        // Whitespace-only and non-string alias values carry no signal: fall
        // through to shape inference for `operation`, nothing for `path`.
        let raw = serde_json::json!({ "op": "   ", "file": 42, "path": "a.rs" })
            .as_object()
            .unwrap()
            .clone();
        let inferred = infer_missing_arguments(&raw, &missing(&["operation"]));
        assert_eq!(inferred[0].1, serde_json::json!("read"));

        let raw = serde_json::json!({ "file": 42 }).as_object().unwrap().clone();
        assert!(infer_missing_arguments(&raw, &missing(&["path"])).is_empty());
    }

    #[test]
    fn no_path_signal_yields_no_inference() {
        // No path/content/alias: nothing grounds `operation`, and a `path` is
        // never invented — the guard must reject instead of guessing.
        let raw = serde_json::Map::new();
        assert!(infer_missing_arguments(&raw, &missing(&["operation", "path"])).is_empty());
    }

    #[test]
    fn resolved_fields_are_not_reinferred() {
        let raw = serde_json::json!({ "op": "write" }).as_object().unwrap().clone();
        assert!(infer_missing_arguments(&raw, &missing(&["path"])).is_empty());
    }

    #[test]
    fn directory_like_shape_test() {
        assert!(is_directory_like("."));
        assert!(is_directory_like("src/"));
        assert!(is_directory_like(" src/ "));
        assert!(!is_directory_like("src/main.rs"));
        assert!(!is_directory_like("src"));
    }

    // -- `write` alias tool ----------------------------------------------------

    use concerto_core::error::PolicyError;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::types::{CapabilitySet, Condition, PolicyAction, PolicyRule, PolicyVerdict};

    /// Audit sink for policy-parity tests (no persistence).
    struct ParityAudit;

    #[async_trait]
    impl AuditLog for ParityAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    fn action_for<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
        PolicyAction {
            tool_name,
            input,
            session_id: concerto_core::ids::Ulid::new(),
            correlation_id: concerto_core::ids::Ulid::new(),
            capability_requirements: CapabilitySet::default().with_requirement("filesystem"),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
            orchestrator_authority: false,
            path_facts: None,
        }
    }

    async fn verdict_for(
        rules: Vec<PolicyRule>,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> PolicyVerdict {
        let engine = SimplePolicyEngine::new(rules, Arc::new(ParityAudit));
        engine.evaluate(&action_for(tool_name, input), CancellationToken::new()).await.unwrap()
    }

    /// `write` alias canonicalizes to the filesystem write identity.
    #[tokio::test]
    async fn write_alias_policy_view_is_canonical_filesystem_write() {
        let (write_tool, _dir) = {
            let dir = TempDir::new().unwrap();
            let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
            (WriteTool::new(root), dir)
        };
        let input = serde_json::json!({ "path": "src/main.rs", "content": "fn main() {}" });
        let (tool_name, canonical) = write_tool.policy_view(&input);
        assert_eq!(tool_name, "filesystem");
        assert_eq!(canonical["operation"], "write");
        assert_eq!(canonical["path"], "src/main.rs");
        assert_eq!(canonical["content"], "fn main() {}");
    }

    /// The alias and a direct filesystem write produce identical verdicts under
    /// every representative rule shape (allow, deny, approval, read-only).
    #[tokio::test]
    async fn write_alias_has_policy_parity_with_filesystem_write() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let write_tool = WriteTool::new(root);
        let alias_input = serde_json::json!({ "path": "src/main.rs", "content": "x" });
        let (alias_name, alias_canonical) = write_tool.policy_view(&alias_input);
        // The canonical alias view must equal a direct filesystem write's input.
        let fs_input = serde_json::json!({
            "operation": "write",
            "path": "src/main.rs",
            "content": "x"
        });
        assert_eq!(alias_name, "filesystem");
        assert_eq!(alias_canonical, fs_input);

        let rule_sets: Vec<Vec<PolicyRule>> = vec![
            // Default install rules: writes require approval.
            concerto_core::PolicyPresets::default_rules(),
            // Permissive preset: everything filesystem is auto-approved.
            concerto_core::PolicyPresets::permissive(),
            // Explicit deny of the filesystem write operation.
            vec![PolicyRule::AutoDeny(Condition::All(vec![
                Condition::ToolName("filesystem".into()),
                Condition::Operation("write".into()),
            ]))],
            // Read-only auto-approve only: a write falls through to default deny.
            vec![PolicyRule::AutoApprove(Condition::All(vec![
                Condition::ToolName("filesystem".into()),
                Condition::Any(vec![
                    Condition::Operation("read".into()),
                    Condition::Operation("list".into()),
                    Condition::Operation("exists".into()),
                ]),
            ]))],
        ];

        for rules in rule_sets {
            let alias_verdict = verdict_for(rules.clone(), &alias_name, &alias_canonical).await;
            let direct_verdict = verdict_for(rules, "filesystem", &fs_input).await;
            assert_eq!(
                alias_verdict, direct_verdict,
                "the write alias must inherit the exact filesystem-write verdict"
            );
        }
    }

    /// The alias and a direct filesystem write are denied by the same rule.
    #[tokio::test]
    async fn write_alias_denied_where_filesystem_write_is_denied() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let write_tool = WriteTool::new(root);
        let alias_input = serde_json::json!({ "path": "src/main.rs", "content": "x" });
        let (alias_name, alias_canonical) = write_tool.policy_view(&alias_input);

        let deny_write = vec![PolicyRule::AutoDeny(Condition::All(vec![
            Condition::ToolName("filesystem".into()),
            Condition::Operation("write".into()),
        ]))];

        let alias_verdict = verdict_for(deny_write.clone(), &alias_name, &alias_canonical).await;
        let direct_verdict = verdict_for(
            deny_write,
            "filesystem",
            &serde_json::json!({"operation": "write", "path": "x", "content": "y"}),
        )
        .await;
        assert_eq!(alias_verdict, PolicyVerdict::Deny);
        assert_eq!(alias_verdict, direct_verdict);
    }

    /// The alias actually materializes a file through the shared VirtualFs.
    #[tokio::test]
    async fn write_alias_writes_file() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let write_tool = WriteTool::new(root);
        let output = write_tool
            .execute(
                serde_json::json!({ "path": "src/hello.rs", "content": "fn main() {}\n" }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(output.summary.contains("Wrote"), "summary: {}", output.summary);
        assert_eq!(output.data["materialized"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/hello.rs")).unwrap(),
            "fn main() {}\n"
        );
    }

    /// The alias shares the canonical tool's VirtualFs when constructed shared.
    #[tokio::test]
    async fn write_alias_shares_vfs_with_filesystem_tool() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        let write_tool = WriteTool::new_shared(root.clone(), vfs.clone());
        let fs_tool = FilesystemTool::new_shared(root, vfs.clone());
        let session = session_for(dir.path());

        write_tool
            .execute(
                serde_json::json!({ "path": "shared.txt", "content": "shared" }),
                &test_policy(),
                &session,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let read = fs_tool
            .execute(
                serde_json::json!({ "operation": "read", "path": "shared.txt" }),
                &test_policy(),
                &session,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(read.data["content"], "shared");
    }

    /// A missing `content` is rejected with the canonical write diagnostic.
    #[tokio::test]
    async fn write_alias_missing_content_is_rejected() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let write_tool = WriteTool::new(root);
        let error = write_tool
            .execute(
                serde_json::json!({ "path": "src/main.rs" }),
                &test_policy(),
                &session_for(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("missing 'content' field for write operation"),
            "unexpected error: {error}"
        );
    }

    // ---- Path-shaped structured facts (audit observability) ----------------

    /// Every read/write/delete/exists/list operation records its operation name
    /// and the attempted plus confined path.
    #[test]
    fn path_facts_records_operation_and_confined_path_for_each_operation() {
        let (tool, dir) = tool_and_dir();
        let session = session_for(dir.path());
        for operation in ["read", "write", "delete", "exists", "list"] {
            let input = serde_json::json!({"operation": operation, "path": "sub/file.txt"});
            let facts = tool.path_facts(&input, &session).expect("facts are produced");
            assert_eq!(facts.operation, operation);
            assert_eq!(facts.attempted_path.as_deref(), Some("sub/file.txt"));
            let resolved = facts.resolved_path.expect("the path resolves inside the workspace");
            assert!(
                // Compare as paths, not as strings: the confined path is
                // canonical (Windows returns the verbatim `\\?\C:\...` form),
                // so its separator is `\` while the input's is `/`. `Path`
                // parses both, `str::ends_with` does not.
                std::path::Path::new(&resolved).ends_with("sub/file.txt"),
                "resolved path {resolved} must be the confined target"
            );
        }
    }

    /// Move and copy also record the attempted and confined destination.
    #[test]
    fn path_facts_records_destination_for_move_and_copy() {
        let (tool, dir) = tool_and_dir();
        let session = session_for(dir.path());
        for operation in ["move", "copy"] {
            let input = serde_json::json!({"operation": operation, "path": "a.txt", "destination": "b.txt"});
            let facts = tool.path_facts(&input, &session).expect("facts are produced");
            assert_eq!(facts.operation, operation);
            assert_eq!(facts.attempted_destination.as_deref(), Some("b.txt"));
            let resolved =
                facts.resolved_destination.expect("the destination resolves in the workspace");
            assert!(resolved.ends_with("b.txt"), "resolved destination: {resolved}");
        }
    }

    /// A containment rewrite is visible as attempted != resolved.
    #[test]
    fn path_facts_show_containment_rewrite() {
        let (tool, dir) = tool_and_dir();
        let session = session_for(dir.path());
        let input = serde_json::json!({"operation": "read", "path": "nested/../target.txt"});
        let facts = tool.path_facts(&input, &session).expect("facts are produced");
        assert_eq!(facts.attempted_path.as_deref(), Some("nested/../target.txt"));
        let resolved = facts.resolved_path.expect("resolved");
        assert_ne!(resolved, "nested/../target.txt", "the resolved path is re-anchored");
        assert!(resolved.ends_with("target.txt"), "resolved path: {resolved}");
    }

    /// A traversal rejection still records the attempted path, with no resolved
    /// path, so the rejected target stays attributable.
    #[test]
    fn path_facts_record_rejected_traversal_without_resolved_path() {
        let (tool, dir) = tool_and_dir();
        let session = session_for(dir.path());
        let input = serde_json::json!({"operation": "read", "path": "../../etc/passwd"});
        let facts = tool.path_facts(&input, &session).expect("facts are produced");
        assert_eq!(facts.attempted_path.as_deref(), Some("../../etc/passwd"));
        assert!(facts.resolved_path.is_none(), "an escaping path must not resolve");
    }

    /// The `write` alias records the canonical filesystem write operation.
    #[test]
    fn write_alias_path_facts_report_canonical_write() {
        let dir = TempDir::new().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let write_tool = WriteTool::new(root);
        let session = session_for(dir.path());
        let raw = serde_json::json!({"path": "alias.txt", "content": "hi"});
        let (_, canonical) = write_tool.policy_view(&raw);
        let facts = write_tool.path_facts(&canonical, &session).expect("facts are produced");
        assert_eq!(facts.operation, "write");
        assert_eq!(facts.attempted_path.as_deref(), Some("alias.txt"));
        assert!(facts.resolved_path.is_some());
    }
}
