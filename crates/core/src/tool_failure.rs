//! ADR-82 slice 2 — typed tool-failure envelope (`ToolFailure`).
//!
//! A bounded, serializable failure record that survives the model boundary,
//! so no consumer parses `Display` text to classify a native failure.
//!
//! Layering: this module names core-and-below types only
//! ([`crate::error::ToolError`], [`crate::types::strip_url_secrets`],
//! [`crate::sanitizer::SecretSanitizer`]). It never names `shell`,
//! `orchestrator`, `memory`, or `sessions` types. Conversions *into* this
//! vocabulary live in the crates that own the source types (e.g.
//! `CommandResult::to_tool_failure()` in `concerto-shell`).
//!
//! Trust rule: sharing this shape grants no trust. A failure whose
//! [`ToolOrigin`] is not [`ToolOrigin::Builtin`] was not classified from
//! Concerto's own code, so [`ToolFailure::from_error`] degrades it to the
//! opaque envelope (`exec.opaque` / `Opaque` / `Unknown`, `opaque: true`).
//! Repair and diagnosis must refuse to route on [`ToolFailureCategory::Opaque`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{
    is_deterministic_failure_message, ToolError, DETERMINISTIC_CONTAINMENT_MARKERS,
    DETERMINISTIC_FAILURE_MARKERS,
};

/// Current wire-schema version for [`ToolFailure`].
pub const TOOL_FAILURE_SCHEMA_VERSION: u16 = 1;

/// Maximum serialized `detail` payload in bytes. Larger payloads are replaced
/// by a content fingerprint (`{ "hash": ..., "len": ... }`), following the
/// `tool_facts::canonical_args` hash+len rule (reimplemented here because
/// core may not name orchestrator types).
pub const TOOL_FAILURE_DETAIL_LIMIT_BYTES: usize = 4096;

/// Maximum `message` length in bytes (truncated on a char boundary after
/// sanitization).
pub const TOOL_FAILURE_MESSAGE_LIMIT_BYTES: usize = 512;

/// Outcome shape of a tool execution. Mirrors the *shape* of the shell
/// envelope's `CommandStatus` without naming it (dependency-layering rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolStatus {
    Succeeded,
    SucceededWithWarnings,
    RecoverableFailure,
    AwaitingApproval,
    Blocked,
    Cancelled,
    TerminalFailure,
}

/// Severity of one structured failure diagnostic. Mirrors the *shape* of the
/// shell envelope's `DiagnosticSeverity` without naming it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolSeverity {
    Info,
    Warning,
    Error,
}

/// One stable, machine-readable explanation attached to a [`ToolFailure`].
/// Mirrors the *shape* of the shell envelope's `Diagnostic`
/// (`{code, severity, message, help}`) without naming it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    pub code: String,
    pub severity: ToolSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// Failure class. `Opaque` marks a failure that arrived without a native
/// typed code (unknown variants, plugin/MCP bridges, unclassifiable shell
/// envelopes) — repair/diagnosis must refuse to route on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolFailureCategory {
    Policy,
    Execution,
    Environment,
    Control,
    Workspace,
    Opaque,
}

/// Recovery disposition. Deliberately not retryable/not-retryable: cancelled
/// and awaiting-decision are neither retryable nor deterministic, so they get
/// their own values and a consumer never has to guess. "Not deterministic"
/// never implies "retryable" — unclassified faults report [`Self::Unknown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ToolDisposition {
    /// The identical action with identical input and identical workspace
    /// state cannot succeed. Do not re-issue.
    DoNotReIssue,
    /// A deliberate stop. Never silently re-issue the same intent; only a
    /// new user instruction may.
    DoNotReIssueSameIntent,
    /// Parked awaiting the operator. Resolution is the answer, not a retry.
    AwaitingDecision,
    /// May resolve on a later attempt. Retry with backoff under a bound.
    BoundedRetryWithBackoff,
    /// A genuine overlay conflict. May resolve after the agent reconciles
    /// the workspace.
    RetryableAfterRebase,
    /// Unclassified. Neither retryable nor deterministic.
    Unknown,
}

/// Where a failure was classified. Serializes as
/// `builtin | plugin:<id> | mcp:<server>:<tool>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolOrigin {
    #[default]
    Builtin,
    Plugin(String),
    Mcp {
        server: String,
        tool: String,
    },
}

impl ToolOrigin {
    /// Whether this failure was classified from Concerto's own code.
    /// Anything else degrades to the opaque envelope (trust rule above).
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        matches!(self, Self::Builtin)
    }

    /// Derive the origin from a registered tool name. Bridge tools are
    /// namespaced (`mcp:<server>:<tool>`); WASM plugin tools carry the
    /// `plugin:` prefix; everything else is builtin.
    #[must_use]
    pub fn for_tool_name(tool_name: &str) -> Self {
        if let Some(rest) = tool_name.strip_prefix("mcp:") {
            let mut parts = rest.splitn(2, ':');
            let server = parts.next().unwrap_or_default().to_string();
            let tool = parts.next().unwrap_or_default().to_string();
            Self::Mcp { server, tool }
        } else if let Some(rest) = tool_name.strip_prefix("plugin:") {
            Self::Plugin(rest.to_string())
        } else {
            Self::Builtin
        }
    }
}

impl std::fmt::Display for ToolOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin => write!(f, "builtin"),
            Self::Plugin(id) => write!(f, "plugin:{id}"),
            Self::Mcp { server, tool } => write!(f, "mcp:{server}:{tool}"),
        }
    }
}

impl std::str::FromStr for ToolOrigin {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw == "builtin" {
            Ok(Self::Builtin)
        } else if let Some(rest) = raw.strip_prefix("mcp:") {
            let mut parts = rest.splitn(2, ':');
            let server = parts.next().unwrap_or_default().to_string();
            let tool = parts.next().unwrap_or_default().to_string();
            if server.is_empty() {
                return Err(format!("invalid tool origin '{raw}': empty MCP server"));
            }
            Ok(Self::Mcp { server, tool })
        } else if let Some(rest) = raw.strip_prefix("plugin:") {
            if rest.is_empty() {
                return Err(format!("invalid tool origin '{raw}': empty plugin id"));
            }
            Ok(Self::Plugin(rest.to_string()))
        } else {
            Err(format!(
                "invalid tool origin '{raw}': expected builtin | plugin:<id> | mcp:<server>:<tool>"
            ))
        }
    }
}

impl Serialize for ToolOrigin {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ToolOrigin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

/// The workspace resource a failure is attributed to: a project path or an
/// opaque pointer (e.g. a URL-derived target that never resolves locally).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolResource {
    Path(String),
    Pointer(String),
}

/// Caller-supplied context for [`ToolFailure::from_error`]. All fields are
/// optional; the error variant supplies the typed code, category,
/// disposition, and status. Construct with [`Self::builtin`] /
/// [`Self::with_origin`] plus the `with_*` builders so adding a field stays
/// additive for downstream crates.
#[derive(Clone, Debug, Default)]
pub struct ToolFailureContext {
    /// Where the failure was classified. Non-builtin origins degrade to the
    /// opaque envelope (trust rule above).
    pub origin: ToolOrigin,
    /// Run correlation id for log joins.
    pub correlation_id: Option<String>,
    /// Evidence-row id this failure is recorded under, when known.
    pub evidence_id: Option<String>,
    /// Bounded structured detail (4 KiB cap with hash+len fallback).
    pub detail: Option<serde_json::Value>,
    /// Attributed workspace resource.
    pub resource: Option<ToolResource>,
    /// Allowed values, for unsatisfiable-input failures.
    pub valid_values: Vec<String>,
}

impl ToolFailureContext {
    /// Context for a natively classified failure.
    #[must_use]
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Context for a failure classified outside Concerto's own code.
    #[must_use]
    pub fn with_origin(origin: ToolOrigin) -> Self {
        Self { origin, ..Self::default() }
    }

    /// Attach a run correlation id for log joins.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }

    /// Attach the evidence-row id this failure is recorded under.
    #[must_use]
    pub fn with_evidence_id(mut self, evidence_id: impl Into<String>) -> Self {
        self.evidence_id = Some(evidence_id.into());
        self
    }

    /// Attach bounded structured detail (4 KiB cap with hash+len fallback).
    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Attribute the failure to a workspace resource.
    #[must_use]
    pub fn with_resource(mut self, resource: ToolResource) -> Self {
        self.resource = Some(resource);
        self
    }

    /// Attach allowed values, for unsatisfiable-input failures.
    #[must_use]
    pub fn with_valid_values(mut self, valid_values: Vec<String>) -> Self {
        self.valid_values = valid_values;
        self
    }
}

/// Typed, bounded failure envelope (ADR-82 §1).
///
/// Wire form: additive fields, unknown fields ignored; unknown
/// `schema_version` values are rejected by [`Self::from_json`]. Rust API:
/// new fields are additive; the enums are `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFailure {
    pub schema_version: u16,
    /// Stable code in `<area>.<name>` grammar (e.g. `exec.timeout`).
    /// Shell-envelope codes (`shell.*`) are adopted verbatim by the shell
    /// conversion, never re-minted.
    pub code: String,
    pub category: ToolFailureCategory,
    pub status: ToolStatus,
    pub diagnostics: Vec<ToolDiagnostic>,
    pub disposition: ToolDisposition,
    /// Bounded structured detail (4 KiB cap with hash+len fallback).
    #[serde(default)]
    pub detail: serde_json::Value,
    #[serde(default)]
    pub resource: Option<ToolResource>,
    #[serde(default)]
    pub valid_values: Vec<String>,
    /// Sanitized, 512-byte-capped human summary. Never parsed for routing.
    pub message: String,
    /// True when the failure was not classified from Concerto's own code.
    pub opaque: bool,
    pub origin: ToolOrigin,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub evidence_id: Option<String>,
}

/// Total per-variant mapping from [`ToolError`] to envelope parts.
///
/// Returns `None` for unknown future variants (the enum is
/// `#[non_exhaustive]`) so the caller degrades to the opaque envelope.
/// `#[allow(unreachable_patterns)]`: within this crate the match is
/// exhaustive today, so the wildcard is unreachable — until a future
/// variant lands, at which point it becomes the safe-degradation arm
/// instead of a compile break. New native variants must still gain an
/// explicit arm above (the totality test fails otherwise).
type NativeEnvelopeParts = (
    &'static str,
    ToolFailureCategory,
    ToolDisposition,
    String,
    serde_json::Value,
    Option<&'static str>,
);

impl ToolFailure {
    /// Total constructor: an exhaustive per-variant match over [`ToolError`]
    /// to typed codes + dispositions. Unknown future variants (the enum is
    /// `#[non_exhaustive]`) degrade to
    /// `{code: "exec.opaque", category: Opaque, disposition: Unknown}`.
    ///
    /// The substring predicate ([`is_deterministic_failure_message`]) is used
    /// ONLY in the opaque-fallback arms below — never as the derivation
    /// source for native variants. The one native arm that must read message
    /// text (`ExecutionFailed`, stringly until H02) derives from the shared
    /// marker constants instead, so envelope and ledger key on one grammar.
    #[must_use]
    pub fn from_error(error: &ToolError, ctx: ToolFailureContext) -> Self {
        if !ctx.origin.is_builtin() {
            return Self::opaque_envelope(&error.to_string(), ctx);
        }
        match Self::native_envelope_parts(error) {
            Some((code, category, disposition, message, detail, help)) => {
                Self::native_envelope(code, category, disposition, message, detail, help, ctx)
            }
            // Unknown future variant (`#[non_exhaustive]`): safe degradation.
            // The ONLY arm that consults the prose predicate, as a
            // compatibility fallback for opaque failure text.
            //
            // TOTALITY SAFEGUARD: adding a `ToolError` variant REQUIRES (1) an
            // explicit arm in `native_envelope_parts` (the wildcard there
            // would otherwise silently degrade the new variant to this
            // opaque envelope) and (2) a case in the
            // `from_error_maps_every_native_variant` table below.
            // `native_envelope_match_is_exhaustive_over_tool_error` fails to
            // COMPILE when (1) is missed; the table fails at runtime when
            // (2) is.
            None => Self::opaque_envelope(&error.to_string(), ctx),
        }
    }

    /// Assemble a natively classified envelope from total-constructor parts.
    fn native_envelope(
        code: &'static str,
        category: ToolFailureCategory,
        disposition: ToolDisposition,
        message: String,
        detail: serde_json::Value,
        help: Option<&'static str>,
        ctx: ToolFailureContext,
    ) -> Self {
        let status = status_for_disposition(disposition);
        let message = sanitize_message(&message);
        // Caller-supplied detail wins when present (the call site knows the
        // failure best); otherwise the variant's structured detail rides.
        let detail = bound_detail(&sanitize_value(&ctx.detail.unwrap_or(detail)));
        let diagnostics = vec![ToolDiagnostic {
            code: code.to_string(),
            severity: ToolSeverity::Error,
            message: message.clone(),
            help: help.map(str::to_string),
        }];
        Self {
            schema_version: TOOL_FAILURE_SCHEMA_VERSION,
            code: code.to_string(),
            category,
            status,
            diagnostics,
            disposition,
            detail,
            resource: ctx.resource,
            valid_values: ctx.valid_values,
            message,
            opaque: false,
            origin: ctx.origin,
            correlation_id: ctx.correlation_id,
            evidence_id: ctx.evidence_id,
        }
    }
    /// Total per-variant mapping from [`ToolError`] to envelope parts:
    /// `(code, category, disposition, raw message, raw detail, help)`.
    ///
    /// Returns `None` for unknown future variants (the enum is
    /// `#[non_exhaustive]`) so the caller degrades to the opaque envelope.
    /// `#[allow(unreachable_patterns)]`: within this crate the match is
    /// exhaustive today, so the wildcard is unreachable — until a future
    /// variant lands, at which point it becomes the safe-degradation arm
    /// instead of a compile break. New native variants must still gain an
    /// explicit arm above (the totality test fails otherwise).
    #[allow(unreachable_patterns)]
    fn native_envelope_parts(error: &ToolError) -> Option<NativeEnvelopeParts> {
        match error {
        ToolError::PolicyDenied { rule } => Some((
            "policy.denied",
                ToolFailureCategory::Policy,
                ToolDisposition::DoNotReIssue,
                format!("policy denied: {rule}"),
                serde_json::json!({ "rule": rule }),
                None,
            )),
            ToolError::PausedAwaitingApproval { tool_name, detail, input_hash, timeout_secs, .. } => Some((
                "policy.awaiting_approval",
                ToolFailureCategory::Policy,
                ToolDisposition::AwaitingDecision,
                format!("awaiting approval for '{tool_name}' (timed out after {timeout_secs}s)"),
                serde_json::json!({
                    "tool_name": tool_name,
                    "detail": detail,
                    "input_hash": input_hash,
                    "timeout_secs": timeout_secs,
                }),
                None,
            )),
            // Native execution fault. The disposition derives from the SHARED
            // marker grammar (`DETERMINISTIC_FAILURE_MARKERS` — the same
            // constants the ledger's `is_deterministic_failure` parses), so
            // the envelope and the ledger can never disagree on this, the
            // most common native failure shape: marker match →
            // `DoNotReIssue`, no match → `Unknown` (never a fabricated retry
            // claim). The grammar is referenced, never copied.
            // INTERIM pending H02 construction-site typing: tools should
            // construct a typed `ToolError::Io` instead of the stringly
            // `ExecutionFailed`, which would retire message matching on this
            // native path entirely. Until then the prose predicate remains
            // the fallback for OPAQUE origins only (non-builtin origins and
            // unknown future variants — unchanged path, see
            // `opaque_envelope`).
            ToolError::ExecutionFailed { message } => Some((
                "exec.execution_failed",
                ToolFailureCategory::Execution,
                if matches_markers(message, DETERMINISTIC_FAILURE_MARKERS) {
                    ToolDisposition::DoNotReIssue
                } else {
                    ToolDisposition::Unknown
                },
                message.clone(),
                serde_json::Value::Null,
                None,
            )),
            ToolError::NotARepository { message } => Some((
                "env.not_a_repository",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
                message.clone(),
                serde_json::Value::Null,
                None,
            )),
            ToolError::Timeout { timeout_secs } => Some((
                "exec.timeout",
                ToolFailureCategory::Execution,
                ToolDisposition::BoundedRetryWithBackoff,
                format!("operation timed out after {timeout_secs}s"),
                serde_json::json!({ "timeout_secs": timeout_secs }),
                Some("Retry with backoff under a bound; raise the timeout if the work is legitimately slow."),
            )),
            ToolError::Cancelled => Some((
                "control.cancelled",
                ToolFailureCategory::Control,
                ToolDisposition::DoNotReIssueSameIntent,
                "cancelled".to_string(),
                serde_json::Value::Null,
                None,
            )),
            ToolError::VirtualFsConflict { path, reason } => Some({
                let containment = is_containment_reason(reason);
                (
                    "workspace.conflict",
                    ToolFailureCategory::Workspace,
                    if containment {
                        ToolDisposition::DoNotReIssue
                    } else {
                        ToolDisposition::RetryableAfterRebase
                    },
                    format!("virtual fs conflict on {path}: {reason}"),
                    serde_json::json!({ "path": path.to_string(), "reason": reason }),
                    if containment {
                        None
                    } else {
                        Some("Reconcile the workspace state (re-read the conflicting path), then re-issue.")
                    },
                )
            }),
            ToolError::RollbackNotSupported => Some((
                "env.rollback_unsupported",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
                "rollback not supported by this tool".to_string(),
                serde_json::Value::Null,
                None,
            )),
            // Slice-2 review fix: `BoundedRetryWithBackoff` OVER-CLAIMED —
            // this variant also carries permanent faults (missing server
            // binary, protocol mismatch, closed response channel) and the
            // variant has no structural signal to split those from transient
            // ones. `Unknown` states "neither retryable nor deterministic"
            // honestly; calibration of the sub-cases is deferred (it needs
            // typed sub-kinds, i.e. construction-site work beyond slice 2).
            // Loop behavior is unchanged: the ledger predicate still returns
            // `false` for `LspError`, so a retry remains allowed exactly as
            // before — only the envelope stops promising it.
            ToolError::LspError { message } => Some((
                "exec.lsp_error",
                ToolFailureCategory::Execution,
                ToolDisposition::Unknown,
                message.clone(),
                serde_json::Value::Null,
                None,
            )),
            ToolError::Io(error) => Some({
                let kind = error.kind();
                let structural = matches!(
                    kind,
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::AlreadyExists
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::IsADirectory
                );
                (
                    "exec.io_error",
                    ToolFailureCategory::Execution,
                    if structural {
                        ToolDisposition::DoNotReIssue
                    } else {
                        ToolDisposition::BoundedRetryWithBackoff
                    },
                    error.to_string(),
                    serde_json::json!({ "io_kind": format!("{kind:?}") }),
                    None,
                )
            }),
            // Unknown future variant (`#[non_exhaustive]`): safe degradation
            // (see the fn docs). `None` routes to the opaque envelope, the
            // ONLY path that consults the prose predicate.
            // TOTALITY: a new `ToolError` variant REQUIRES an explicit arm
            // above (plus a case in the `from_error_maps_every_native_variant`
            // table) — this wildcard must not become its resting place.
            // `native_envelope_match_is_exhaustive_over_tool_error` fails to
            // compile until the arm exists.
            _ => None,
        }
    }

    /// Opaque envelope for failures not classified from Concerto's own code
    /// (plugin/MCP bridges). The native-looking classification is untrusted
    /// by definition, so the code degrades to `exec.opaque` rather than
    /// masquerading as native. The prose predicate is the compatibility
    /// fallback for the disposition only.
    fn opaque_envelope(raw_message: &str, ctx: ToolFailureContext) -> Self {
        let disposition = if is_deterministic_failure_message(raw_message) {
            ToolDisposition::DoNotReIssue
        } else {
            ToolDisposition::Unknown
        };
        let message = sanitize_message(raw_message);
        Self {
            schema_version: TOOL_FAILURE_SCHEMA_VERSION,
            code: "exec.opaque".to_string(),
            category: ToolFailureCategory::Opaque,
            status: status_for_disposition(disposition),
            diagnostics: vec![ToolDiagnostic {
                code: "exec.opaque".to_string(),
                severity: ToolSeverity::Error,
                message: message.clone(),
                help: None,
            }],
            disposition,
            detail: bound_detail(&ctx.detail.unwrap_or(serde_json::Value::Null)),
            resource: ctx.resource,
            valid_values: ctx.valid_values,
            message,
            opaque: true,
            origin: ctx.origin,
            correlation_id: ctx.correlation_id,
            evidence_id: ctx.evidence_id,
        }
    }

    /// Deserialize with schema-version enforcement: the current version loads
    /// as-is; unknown (future) versions are rejected, never silently
    /// accepted. Unknown *fields* are ignored (additive wire form).
    pub fn from_json(json: &str) -> Result<Self, String> {
        let failure: Self = serde_json::from_str(json)
            .map_err(|error| format!("failed to deserialize tool failure: {error}"))?;
        if failure.schema_version != TOOL_FAILURE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported tool-failure schema version {}: this runtime supports v{}",
                failure.schema_version, TOOL_FAILURE_SCHEMA_VERSION
            ));
        }
        Ok(failure)
    }
}

/// Derive the envelope status from the disposition: deterministic refusals
/// block, cancellations cancel, approvals wait; everything else is
/// recoverable (permits continuation — only a terminal status would stop
/// the loop, and an unclassified fault must not stop it).
fn status_for_disposition(disposition: ToolDisposition) -> ToolStatus {
    match disposition {
        ToolDisposition::DoNotReIssue => ToolStatus::Blocked,
        ToolDisposition::DoNotReIssueSameIntent => ToolStatus::Cancelled,
        ToolDisposition::AwaitingDecision => ToolStatus::AwaitingApproval,
        ToolDisposition::BoundedRetryWithBackoff
        | ToolDisposition::RetryableAfterRebase
        | ToolDisposition::Unknown => ToolStatus::RecoverableFailure,
    }
}

/// Whether a `VirtualFsConflict` reason is a containment/traversal rejection
/// (workspace boundary fact — the identical command is rejected again) as
/// opposed to a genuine overlay conflict (retryable after a rebase).
/// Reads the shared `DETERMINISTIC_CONTAINMENT_MARKERS` grammar
/// (`crate::error`) — the single definition site, no local copy.
fn is_containment_reason(reason: &str) -> bool {
    matches_markers(reason, DETERMINISTIC_CONTAINMENT_MARKERS)
}

/// Whether `text` contains any of `markers`, compared lowercased — the one
/// fold over the shared marker grammar from `crate::error`. The marker lists
/// are referenced, never copied, so the envelope cannot drift from the
/// ledger.
fn matches_markers(text: &str, markers: &[&str]) -> bool {
    let lowered = text.to_ascii_lowercase();
    markers.iter().any(|marker| lowered.contains(marker))
}

/// Whether a failure code follows the `<area>.<name>` grammar: at least two
/// dot-separated segments (shell codes carry deeper paths), every segment
/// non-empty over `[A-Za-z0-9_-]`.
#[must_use]
pub fn is_valid_code(code: &str) -> bool {
    let mut parts = code.split('.');
    let valid_part = |part: Option<&str>| {
        part.is_some_and(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    };
    if !valid_part(parts.next()) || !valid_part(parts.next()) {
        return false;
    }
    parts.all(|part| {
        !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// Sanitize a human summary: strip secret-bearing URL components, redact
/// secret patterns, then cap at [`TOOL_FAILURE_MESSAGE_LIMIT_BYTES`] bytes.
fn sanitize_message(raw: &str) -> String {
    let stripped = crate::types::strip_url_secrets(raw);
    let redacted = crate::sanitizer::SecretSanitizer::default().sanitize(&stripped);
    truncate_bytes(&redacted, TOOL_FAILURE_MESSAGE_LIMIT_BYTES)
}

/// Recursively sanitize every string in a JSON value (URL components +
/// secret patterns). Structure and non-string scalars pass through.
fn sanitize_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            let stripped = crate::types::strip_url_secrets(text);
            serde_json::Value::String(
                crate::sanitizer::SecretSanitizer::default().sanitize(&stripped),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sanitize_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter().map(|(key, val)| (key.clone(), sanitize_value(val))).collect(),
        ),
        scalar => scalar.clone(),
    }
}

/// Bound a sanitized detail value to [`TOOL_FAILURE_DETAIL_LIMIT_BYTES`]
/// bytes; oversized payloads become a content fingerprint — the
/// `tool_facts::canonical_args` hash+len rule, reimplemented here because
/// core may not name orchestrator types.
fn bound_detail(detail: &serde_json::Value) -> serde_json::Value {
    let compact = serde_json::to_string(detail).unwrap_or_default();
    if compact.len() <= TOOL_FAILURE_DETAIL_LIMIT_BYTES {
        detail.clone()
    } else {
        serde_json::json!({
            "hash": blake3::hash(compact.as_bytes()).to_hex().to_string(),
            "len": compact.len(),
        })
    }
}

/// Truncate to a byte bound on a char boundary (never splits UTF-8).
fn truncate_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_ctx() -> ToolFailureContext {
        ToolFailureContext::builtin()
    }

    #[test]
    fn origin_grammar_round_trips() {
        for origin in [
            ToolOrigin::Builtin,
            ToolOrigin::Plugin("my-plugin".to_string()),
            ToolOrigin::Mcp { server: "github".to_string(), tool: "list_repos".to_string() },
        ] {
            let rendered = origin.to_string();
            let parsed: ToolOrigin =
                rendered.parse().unwrap_or_else(|_| panic!("origin parses: {rendered}"));
            assert_eq!(parsed, origin, "origin grammar round-trips");
            let json = serde_json::to_string(&origin).expect("origin serializes");
            let back: ToolOrigin = serde_json::from_str(&json).expect("origin deserializes");
            assert_eq!(back, origin);
        }
        assert!("bogus".parse::<ToolOrigin>().is_err(), "unknown origins rejected");
        assert!("plugin:".parse::<ToolOrigin>().is_err(), "empty plugin id rejected");
        assert!("mcp::tool".parse::<ToolOrigin>().is_err(), "empty MCP server rejected");
    }

    #[test]
    fn origin_derives_from_registered_tool_names() {
        assert_eq!(ToolOrigin::for_tool_name("shell"), ToolOrigin::Builtin);
        assert_eq!(ToolOrigin::for_tool_name("filesystem"), ToolOrigin::Builtin);
        assert_eq!(
            ToolOrigin::for_tool_name("mcp:github:list_repos"),
            ToolOrigin::Mcp { server: "github".to_string(), tool: "list_repos".to_string() }
        );
        assert_eq!(
            ToolOrigin::for_tool_name("plugin:my-plugin"),
            ToolOrigin::Plugin("my-plugin".to_string())
        );
    }

    /// ADR-82 §1 per-variant table: every native `ToolError` variant maps to
    /// its typed code, category, and disposition with a valid `<area>.<name>`
    /// code — the totality test over the taxonomy.
    #[test]
    fn from_error_maps_every_native_variant() {
        let correlation = crate::ids::Ulid::new();
        let cases: Vec<(ToolError, &str, ToolFailureCategory, ToolDisposition)> = vec![
            (
                ToolError::PolicyDenied { rule: "deny_all".into() },
                "policy.denied",
                ToolFailureCategory::Policy,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::PausedAwaitingApproval {
                    tool_name: "shell".into(),
                    detail: "rm -rf /".into(),
                    input_hash: "hash".into(),
                    correlation_id: correlation,
                    timeout_secs: 30,
                },
                "policy.awaiting_approval",
                ToolFailureCategory::Policy,
                ToolDisposition::AwaitingDecision,
            ),
            (
                ToolError::ExecutionFailed { message: "boom".into() },
                "exec.execution_failed",
                ToolFailureCategory::Execution,
                ToolDisposition::Unknown,
            ),
            (
                ToolError::NotARepository { message: "no .git".into() },
                "env.not_a_repository",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Timeout { timeout_secs: 30 },
                "exec.timeout",
                ToolFailureCategory::Execution,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
            (
                ToolError::Cancelled,
                "control.cancelled",
                ToolFailureCategory::Control,
                ToolDisposition::DoNotReIssueSameIntent,
            ),
            (
                ToolError::VirtualFsConflict {
                    path: "../etc/passwd".into(),
                    reason: "path traversal detected — resolved path escapes workspace root".into(),
                },
                "workspace.conflict",
                ToolFailureCategory::Workspace,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::VirtualFsConflict {
                    path: "a.txt".into(),
                    reason: "the file changed since it was read".into(),
                },
                "workspace.conflict",
                ToolFailureCategory::Workspace,
                ToolDisposition::RetryableAfterRebase,
            ),
            (
                ToolError::RollbackNotSupported,
                "env.rollback_unsupported",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
            ),
            (
                // Review fix: `Unknown`, not `bounded-retry-with-backoff` —
                // see the rationale on the arm (and its dedicated test).
                ToolError::LspError { message: "server crashed".into() },
                "exec.lsp_error",
                ToolFailureCategory::Execution,
                ToolDisposition::Unknown,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "deny")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exists")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::NotADirectory, "nad")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::IsADirectory, "iad")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
            (
                ToolError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "slow")),
                "exec.io_error",
                ToolFailureCategory::Execution,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
        ];
        for (error, code, category, disposition) in cases {
            let failure = ToolFailure::from_error(&error, builtin_ctx());
            assert_eq!(failure.code, code, "code for {error}");
            assert_eq!(failure.category, category, "category for {error}");
            assert_eq!(failure.disposition, disposition, "disposition for {error}");
            assert!(is_valid_code(&failure.code), "code grammar for {error}");
            assert!(!failure.opaque, "native failures are not opaque: {error}");
            assert_eq!(failure.origin, ToolOrigin::Builtin);
            assert_eq!(failure.schema_version, TOOL_FAILURE_SCHEMA_VERSION);
            assert!(
                failure.message.len() <= TOOL_FAILURE_MESSAGE_LIMIT_BYTES,
                "message cap for {error}"
            );
            assert_eq!(failure.diagnostics.len(), 1, "one diagnostic for {error}");
            assert_eq!(failure.diagnostics[0].code, code);
        }
    }

    /// Slice-2 review fix (blocking): the native `ExecutionFailed`
    /// disposition and the ledger's `is_deterministic_failure` parse the
    /// SAME shared marker grammar, so the two surfaces agree on the most
    /// common native failure shape — while non-builtin origins keep the
    /// forced opaque path unchanged.
    #[test]
    fn native_execution_failed_agrees_with_the_ledger_grammar() {
        // Marker-shaped native message ("file not found" is in
        // DETERMINISTIC_FAILURE_MARKERS): both surfaces say the identical
        // re-issue cannot succeed.
        let deterministic =
            ToolError::ExecutionFailed { message: "file not found: src/main.rs".into() };
        assert!(
            deterministic.is_deterministic_failure(),
            "ledger classifies the message deterministic"
        );
        let envelope = ToolFailure::from_error(&deterministic, builtin_ctx());
        assert_eq!(envelope.disposition, ToolDisposition::DoNotReIssue, "envelope agrees");
        assert_eq!(envelope.code, "exec.execution_failed", "code unchanged");
        assert_eq!(envelope.category, ToolFailureCategory::Execution);
        assert!(!envelope.opaque, "native origin stays native");
        assert_eq!(envelope.status, ToolStatus::Blocked, "deterministic refusals block");

        // The identical text from a non-builtin origin still takes the
        // forced opaque path (trust rule): code degrades, the disposition
        // remains predicate-driven — unchanged behavior.
        let opaque = ToolFailure::from_error(
            &ToolError::ExecutionFailed { message: "file not found: src/main.rs".into() },
            ToolFailureContext::with_origin(ToolOrigin::Plugin("evil".into())),
        );
        assert!(opaque.opaque, "non-builtin origins are opaque");
        assert_eq!(opaque.code, "exec.opaque");
        assert_eq!(opaque.category, ToolFailureCategory::Opaque);
        assert_eq!(opaque.disposition, ToolDisposition::DoNotReIssue);
        assert_eq!(opaque.origin, ToolOrigin::Plugin("evil".into()));

        // Unparseable native text: no marker match, so the envelope reports
        // `Unknown` — the same "not deterministic" verdict as the ledger,
        // with no fabricated retry claim in either direction.
        let unknown =
            ToolError::ExecutionFailed { message: "the model produced malformed JSON".into() };
        assert!(!unknown.is_deterministic_failure(), "ledger says not deterministic");
        let envelope = ToolFailure::from_error(&unknown, builtin_ctx());
        assert_eq!(envelope.disposition, ToolDisposition::Unknown, "envelope agrees");
        assert_eq!(envelope.code, "exec.execution_failed");
        assert!(!envelope.opaque);
        assert_eq!(envelope.status, ToolStatus::RecoverableFailure, "unclassified never blocks");
    }

    /// Slice-2 review fix (high): `LspError` stops over-claiming
    /// `bounded-retry-with-backoff`. Rationale: the variant carries
    /// heterogeneous faults — permanent ones (missing binary, protocol
    /// mismatch, closed response channel) alongside transient ones — and
    /// exposes no structural field to split on, so the envelope reports
    /// `Unknown` ("neither retryable nor deterministic") until calibration
    /// gets typed sub-kinds. Loop behavior is deliberately unchanged: the
    /// ledger predicate still returns `false` for `LspError`, so a retry
    /// remains allowed exactly as before.
    #[test]
    fn lsp_error_reports_unknown_instead_of_over_claiming_retry() {
        for message in ["lsp server binary not found on PATH", "protocol mismatch: bad handshake"] {
            let error = ToolError::LspError { message: message.into() };
            assert!(!error.is_deterministic_failure(), "ledger unchanged: retry stays allowed");
            let failure = ToolFailure::from_error(&error, builtin_ctx());
            assert_eq!(failure.disposition, ToolDisposition::Unknown, "for {message}");
            assert_eq!(failure.code, "exec.lsp_error");
            assert_eq!(failure.category, ToolFailureCategory::Execution);
            assert!(!failure.opaque, "native origin stays native");
            assert_eq!(
                failure.status,
                ToolStatus::RecoverableFailure,
                "an unclassified fault must not stop anything"
            );
        }
    }

    /// Totality safeguard, compile-time half: this match deliberately has NO
    /// wildcard arm, so adding a `ToolError` variant without handling it in
    /// `ToolFailure::native_envelope_parts` fails `cargo test` (and
    /// `clippy --all-targets`) at COMPILE time. The runtime half is
    /// [`from_error_maps_every_native_variant`], which fails when the new
    /// variant has an arm but no table case.
    #[test]
    fn native_envelope_match_is_exhaustive_over_tool_error() {
        fn covered(error: &ToolError) {
            match error {
                ToolError::PolicyDenied { .. }
                | ToolError::PausedAwaitingApproval { .. }
                | ToolError::ExecutionFailed { .. }
                | ToolError::NotARepository { .. }
                | ToolError::Timeout { .. }
                | ToolError::Cancelled
                | ToolError::VirtualFsConflict { .. }
                | ToolError::RollbackNotSupported
                | ToolError::LspError { .. }
                | ToolError::Io(_) => {}
            }
        }
        covered(&ToolError::Cancelled);
    }

    #[test]
    fn plugin_and_mcp_origins_degrade_to_opaque() {
        for origin in [
            ToolOrigin::Plugin("evil".to_string()),
            ToolOrigin::Mcp { server: "github".to_string(), tool: "list_repos".to_string() },
        ] {
            // Even a native-looking variant is untrusted outside builtin.
            let failure = ToolFailure::from_error(
                &ToolError::Timeout { timeout_secs: 5 },
                ToolFailureContext::with_origin(origin.clone()),
            );
            assert!(failure.opaque, "non-builtin origins are opaque");
            assert_eq!(failure.code, "exec.opaque", "no masquerading as native");
            assert_eq!(failure.category, ToolFailureCategory::Opaque);
            assert_eq!(failure.disposition, ToolDisposition::Unknown);
            assert_eq!(failure.origin, origin);

            // The predicate survives as the opaque-fallback disposition only.
            let deterministic = ToolFailure::from_error(
                &ToolError::ExecutionFailed { message: "permission denied: /srv/x".into() },
                ToolFailureContext::with_origin(origin),
            );
            assert!(deterministic.opaque);
            assert_eq!(deterministic.disposition, ToolDisposition::DoNotReIssue);
        }
    }

    #[test]
    fn detail_payloads_are_bounded_by_hash_len_fallback() {
        let big = serde_json::json!({ "blob": "x".repeat(TOOL_FAILURE_DETAIL_LIMIT_BYTES + 1) });
        let failure = ToolFailure::from_error(
            &ToolError::Timeout { timeout_secs: 1 },
            ToolFailureContext { detail: Some(big.clone()), ..ToolFailureContext::builtin() },
        );
        let compact = serde_json::to_string(&big).expect("detail serializes");
        assert_eq!(
            failure.detail,
            serde_json::json!({
                "hash": blake3::hash(compact.as_bytes()).to_hex().to_string(),
                "len": compact.len(),
            }),
            "oversized detail degrades to hash+len"
        );
        let small = serde_json::json!({ "rule": "deny_all" });
        let failure = ToolFailure::from_error(
            &ToolError::PolicyDenied { rule: "deny_all".into() },
            ToolFailureContext { detail: Some(small.clone()), ..ToolFailureContext::builtin() },
        );
        assert_eq!(failure.detail, small, "small detail rides verbatim");
    }

    #[test]
    fn secrets_never_reach_message_or_detail() {
        let url = "https://user:s3cret@example.com/api?token=abc123&x=1#frag";
        let failure = ToolFailure::from_error(
            &ToolError::ExecutionFailed { message: format!("fetch {url} failed") },
            ToolFailureContext {
                detail: Some(serde_json::json!({ "target": url })),
                ..ToolFailureContext::builtin()
            },
        );
        assert!(!failure.message.contains("s3cret"), "userinfo stripped: {}", failure.message);
        assert!(!failure.message.contains("token=abc123"), "query stripped: {}", failure.message);
        assert!(!failure.message.contains("#frag"), "fragment stripped: {}", failure.message);
        assert!(failure.message.contains("https://example.com/api"), "host+path kept");
        let detail = serde_json::to_string(&failure.detail).expect("detail serializes");
        assert!(!detail.contains("s3cret"), "detail stripped: {detail}");
        assert!(!detail.contains("abc123"), "detail query stripped: {detail}");

        let keyed = ToolFailure::from_error(
            &ToolError::ExecutionFailed {
                message: "call failed with api_key=1234567890abcdef1234567890abcdef".into(),
            },
            builtin_ctx(),
        );
        assert!(!keyed.message.contains("1234567890abcdef"), "key redacted: {}", keyed.message);
    }

    #[test]
    fn long_messages_cap_at_512_bytes_on_char_boundaries() {
        let failure = ToolFailure::from_error(
            &ToolError::ExecutionFailed { message: "é".repeat(400) },
            builtin_ctx(),
        );
        assert!(
            failure.message.len() <= TOOL_FAILURE_MESSAGE_LIMIT_BYTES,
            "message capped: {} bytes",
            failure.message.len()
        );
        assert!(failure.message.is_char_boundary(failure.message.len()), "char boundary kept");
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let failure = ToolFailure::from_error(
            &ToolError::Timeout { timeout_secs: 30 },
            ToolFailureContext {
                correlation_id: Some("corr-1".to_string()),
                evidence_id: Some("ev-1".to_string()),
                resource: Some(ToolResource::Path("src/main.rs".to_string())),
                valid_values: vec!["read".to_string()],
                ..ToolFailureContext::builtin()
            },
        );
        let json = serde_json::to_string(&failure).expect("failure serializes");
        let back = ToolFailure::from_json(&json).expect("current version loads");
        assert_eq!(back, failure, "round-trip preserves the envelope");
    }

    #[test]
    fn unknown_future_schema_versions_are_rejected() {
        let mut failure = ToolFailure::from_error(&ToolError::Cancelled, builtin_ctx());
        failure.schema_version = TOOL_FAILURE_SCHEMA_VERSION + 1;
        let json = serde_json::to_string(&failure).expect("failure serializes");
        let err = ToolFailure::from_json(&json).expect_err("future versions rejected");
        assert!(err.contains("unsupported tool-failure schema version"), "{err}");
    }

    #[test]
    fn wire_form_ignores_unknown_fields() {
        let mut json =
            serde_json::to_value(ToolFailure::from_error(&ToolError::Cancelled, builtin_ctx()))
                .expect("failure serializes");
        json["future_field"] = serde_json::json!("ignored");
        let back: ToolFailure = serde_json::from_value(json).expect("unknown fields ignored");
        assert_eq!(back.code, "control.cancelled");
    }

    #[test]
    fn code_grammar_validator_rejects_malformed_codes() {
        assert!(is_valid_code("exec.timeout"));
        assert!(is_valid_code("shell.process.non-zero-exit"));
        assert!(is_valid_code("a.b.c"), "shell codes carry deeper paths");
        assert!(!is_valid_code("noarea"));
        assert!(!is_valid_code(".noname"));
        assert!(!is_valid_code("area."));
        assert!(!is_valid_code("a..b"));
        assert!(!is_valid_code(""));
    }

    /// Legacy decode (ADR-82 R6): summaries recorded before the additive
    /// `failure` key existed decode with `failure: None` — display data,
    /// never trusted evidence.
    #[test]
    fn legacy_summaries_without_failure_key_decode_to_none() {
        let legacy = serde_json::json!({
            "tool_name": "shell",
            "operation": null,
            "path": null,
            "success": false,
            "summary": "boom",
        });
        let summary: crate::types::ToolExecutionSummary =
            serde_json::from_value(legacy).expect("legacy summary decodes");
        assert!(summary.failure.is_none(), "absent key defaults to None");

        let with_failure =
            ToolFailure::from_error(&ToolError::Timeout { timeout_secs: 5 }, builtin_ctx());
        let json = serde_json::to_value(&with_failure).expect("failure serializes");
        let mut current = serde_json::json!({
            "tool_name": "shell",
            "operation": null,
            "path": null,
            "success": false,
            "summary": "timed out",
            "failure": json,
        });
        let summary: crate::types::ToolExecutionSummary =
            serde_json::from_value(current.clone()).expect("current summary decodes");
        assert_eq!(
            summary.failure.as_ref().map(|failure| failure.code.as_str()),
            Some("exec.timeout")
        );
        // Unknown wire fields stay ignored on the summary too.
        current["future_field"] = serde_json::json!(true);
        let summary: crate::types::ToolExecutionSummary =
            serde_json::from_value(current).expect("unknown fields ignored");
        assert!(summary.failure.is_some());
    }
}
